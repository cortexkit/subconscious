use std::{io::Write, process::Command};

use subc_os::windows_acl::{
    create_private_dir, create_private_file,
    test_support::{assert_owner_only, grant_everyone, grant_everyone_read},
    verify_private_dir,
};

use super::*;

fn junction(target: &Path, link: &Path) {
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/j"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rescue_uses_a_data_home_beneath_a_junction_but_refuses_one_at_the_install_root() {
    let mut fake = Fake::new();
    let real = fake.root.with_file_name("real");
    let alias = fake.root.with_file_name("alias");
    fs::create_dir_all(real.join("data")).unwrap();
    junction(&real, &alias);
    fake.root = alias.join("data").join(INSTALL_DIR);
    prepare_with(&fake).unwrap();
    let auth = real.join("data/cortexkit/rescue/pi-profile/auth.json");
    create_private_file(&auth)
        .unwrap()
        .write_all(b"credential")
        .unwrap();
    assert!(forget_with(&fake).unwrap().removed);
    assert!(!auth.exists());

    let fake = Fake::new();
    let outside = fake.root.with_file_name("outside");
    fs::create_dir(&outside).unwrap();
    junction(&outside, &fake.root);
    assert!(fake.error().contains(fake.root.to_str().unwrap()));
    assert!(forget_with(&fake).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn rescue_preserves_existing_install_acl_and_keeps_managed_directories_and_reports_private() {
    let fake = Fake::new();
    create_private_dir(&fake.root).unwrap();
    grant_everyone(&fake.root);
    fs::create_dir_all(fake.profile()).unwrap();
    fs::create_dir_all(fake.tools("npm-cache/_logs")).unwrap();
    let auth = fake.profile().join(AUTH);
    create_private_file(&auth)
        .unwrap()
        .write_all(b"credential")
        .unwrap();
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
        run.session_dir().to_owned(),
    ] {
        verify_private_dir(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    }
    assert_owner_only(&auth, false, true);
    run.write_session_file("report.json", b"report").unwrap();
    assert_owner_only(&run.session_dir().join("report.json"), false, true);
    assert!(forget_with(&fake).unwrap().removed);
    assert!(
        verify_private_dir(&fake.root).is_err(),
        "rescue must not narrow the install root"
    );
}

#[test]
fn cache_log_and_profile_junctions_are_refused_without_touching_target() {
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
        create_private_dir(&outside).unwrap();
        grant_everyone(&outside);
        fs::write(outside.join("sentinel"), b"untouched").unwrap();
        let leaf = fake.root.join(name);
        create_private_dir(leaf.parent().unwrap()).unwrap();
        junction(&outside, &leaf);
        assert!(fake.error().contains(leaf.to_str().unwrap()), "{name}");
        if name == "rescue" || name == "rescue/pi-profile" {
            assert!(forget_with(&fake)
                .err()
                .unwrap()
                .contains(leaf.to_str().unwrap()));
        }
        assert!(verify_private_dir(&outside).is_err());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
    }
}

#[test]
fn redirected_install_and_retention_locks_do_not_publish_or_prune() {
    let fake = Fake::new();
    let outside = fake.root.with_file_name("outside");
    fs::create_dir(&outside).unwrap();
    for version in ["0.1.0", "0.2.0", "0.3.0"] {
        fs::create_dir_all(fake.tools("node").join(version)).unwrap();
    }
    let install = fake.tools("node/.lock");
    junction(&outside, &install);
    assert!(fake.error().contains(".lock"));
    fs::remove_dir(&install).unwrap();
    junction(&outside, &fake.tools("node/.use-0.1.0"));
    assert!(fake.error().contains(".use-0.1.0"));
    assert!(!fake.node_root().exists());
    assert!(fake.tools("node/0.1.0").is_dir());
    assert!(fake.tools("node/0.2.0").is_dir());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    no_staging(&fake.tools("node"));
}

#[test]
fn auth_refuses_a_non_regular_file_and_a_dacl_granting_access_to_other_users() {
    let fake = Fake::new().seeded();
    prepare_with(&fake).unwrap();
    fake.children.lock().unwrap().clear();
    let auth = fake.profile().join(AUTH);
    fs::create_dir(&auth).unwrap();
    assert!(fake.error().contains("auth.json"));
    assert!(forget_with(&fake).is_err());
    fs::remove_dir(&auth).unwrap();
    create_private_file(&auth)
        .unwrap()
        .write_all(b"credential")
        .unwrap();
    grant_everyone_read(&auth);
    assert!(fake.error().contains("another user or group"));
    assert_eq!(fs::read(&auth).unwrap(), b"credential");
    assert!(fake.children.lock().unwrap().is_empty());
}

#[test]
fn rescue_refuses_hard_linked_auth_but_forget_unlinks_only_its_name() {
    let fake = Fake::new().seeded();
    prepare_with(&fake).unwrap();
    let auth = fake.profile().join(AUTH);
    create_private_file(&auth)
        .unwrap()
        .write_all(b"credential")
        .unwrap();
    prepare_with(&fake).unwrap();

    fake.children.lock().unwrap().clear();
    let other_link = fake.root.with_file_name("other-auth");
    fs::hard_link(&auth, &other_link).unwrap();
    let error = fake.error();
    assert!(
        error.ends_with("auth.json: credential must not have another hard link"),
        "{error}"
    );
    assert!(fake.children.lock().unwrap().is_empty());
    assert_eq!(fs::read(&auth).unwrap(), b"credential");

    assert!(forget_with(&fake).unwrap().removed);
    assert!(
        matches!(fs::symlink_metadata(&auth), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    );
    assert_eq!(fs::read(&other_link).unwrap(), b"credential");
}

#[test]
fn package_bin_cannot_escape_pi_install_through_a_link() {
    let mut fake = Fake::new().seeded();
    fake.path_node("v22.19.0", true);
    let package = fake.pi().join("node_modules").join(PI_PACKAGE);
    let target = fake.root.with_file_name("outside-script");
    fs::write(&target, b"outside").unwrap();
    fs::remove_file(package.join(BIN)).unwrap();
    std::os::windows::fs::symlink_file(&target, package.join(BIN)).unwrap();
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

#[test]
fn real_zip_install_verifies_node_keeps_private_acls_and_cleans_staging() {
    let mut fake = Fake::new();
    fake.platform = Platform {
        os: "windows",
        arch: std::env::consts::ARCH,
    };
    let archive = pins::node_archive(fake.platform).unwrap();
    let archive_path = fake.root.with_file_name("node 'archive'.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&archive_path).unwrap());
    let fixture = cortexkit_test_support::cargo_profile_bin("doctor-module-fixture");
    for (relative, bytes) in [
        ("node.exe", fs::read(fixture).unwrap()),
        ("node.version", format!("v{NODE_VERSION}").into_bytes()),
        ("node_modules/npm/bin/npm-cli.js", b"fake npm CLI".to_vec()),
    ] {
        zip.start_file(
            format!("{}/{relative}", archive.folder()),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
    fake.archive_file = Some(archive_path);
    prepare_with(&fake).unwrap_or_else(|error| {
        panic!("{error}; steps: {:?}", fake.events.lock().unwrap());
    });

    let root = fake.node_root();
    assert!(root.join("node.exe").is_file());
    assert!(root.join("node_modules/npm/bin/npm-cli.js").is_file());
    let mut pending = vec![root];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            verify_private_dir(&path).unwrap();
            pending.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else {
            subc_os::windows_acl::verify_owner_only(&fs::File::open(path).unwrap()).unwrap();
        }
    }
    no_staging(&fake.tools("node"));
    assert!(!fake.tools("node").join(archive.filename()).exists());
}

#[tokio::test]
async fn bounded_probe_works_from_an_async_caller_and_enforces_timeout() {
    let fake = Fake::new();
    let system = std::env::var_os("SystemRoot").expect("Windows system root");
    let bin = PathBuf::from(&system).join("System32");
    let probe = |program, args: &[&str], timeout| Child {
        program: bin.join(program),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        cwd: fake.root.parent().unwrap().into(),
        env: vec![("SystemRoot".into(), system.clone())],
        timeout: Some(timeout),
        io: Io::Capture,
    };
    let good = System
        .run(probe(
            "cmd.exe",
            &["/d", "/c", "echo", "probe"],
            Duration::from_secs(2),
        ))
        .unwrap();
    assert_eq!(good.stdout, b"probe\r\n");
    let error = System
        .run(probe(
            "ping.exe",
            &["-n", "10", "127.0.0.1"],
            Duration::from_millis(60),
        ))
        .err()
        .unwrap();
    assert!(error.contains("timed out"), "{error}");
}
