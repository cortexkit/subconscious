use super::*;

#[cfg(windows)]
#[test]
fn setup_commits_and_keeps_working_while_a_retired_ck_image_is_mapped() {
    use std::io::Write;
    let fixture = SetupFixture::installed("ck-retired-journal");
    let public = fixture.data_home.join("cortexkit/cmd/ck.exe");
    fs::create_dir_all(public.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ck-under-test"), &public).unwrap();
    let root = fixture.data_home.join("cortexkit");
    let manifest = root.join("installer-manifest.json");
    let mut ownership: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let original = fs::read(&public).unwrap();
    let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&original));
    ownership["mutations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind":"managed-binary", "path":public, "component":"core", "sha256":digest}));
    fs::write(&manifest, serde_json::to_vec(&ownership).unwrap()).unwrap();
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut document: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    document["modules"]
        .as_object_mut()
        .unwrap()
        .remove("subc-mcp");
    fs::write(&config, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let hold = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    hold.set_nonblocking(true).unwrap();
    let mut old = fixture
        .command_from(Command::new(&public), &index, &["setup", "--dry-run"])
        .env(
            "CK_RELEASE_INDEX_URL",
            format!("http://{}/index.json", hold.local_addr().unwrap()),
        )
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pending = loop {
        match hold.accept() {
            Ok((stream, _)) => break Some(stream),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                if old.try_wait().unwrap().is_some() {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => break None,
        }
    };
    let result = {
        assert!(
            pending.is_some(),
            "old ck did not reach the fixture's hold point"
        );
        let temp = fixture._root.path().join("download-temp");
        fs::create_dir(&temp).unwrap();
        let stage = temp.join("verified");
        subc_os::create_private_directory_exclusive(&stage).unwrap();
        let extracted = stage.join("ck.exe");
        fs::copy(&public, &extracted).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&extracted)
            .unwrap()
            .write_all(b"new release bytes")
            .unwrap();
        let placement = fixture
            .command_from(
                Command::new(&extracted),
                &index,
                &["__install-bootstrap", &"a".repeat(64), "1.2.3"],
            )
            .env("TMP", &temp)
            .env("TEMP", &temp)
            .output()
            .unwrap();
        assert_exit(&placement, 0);
        let setup = || {
            fixture
                .command_from(Command::new(&public), &index, &["setup", "--yes"])
                .output()
                .unwrap()
        };
        assert_exit(&setup(), 0);
        assert!(subc_jsonc::read_document(&config).unwrap().1["modules"]["subc-mcp"].is_object());
        let retired = root.join("installer.retired");
        assert_eq!(fs::read_dir(&retired).unwrap().count(), 1);
        assert!(!root.join("installer.journal").exists());
        let next = setup();
        assert_exit(&next, 0);
        assert!(text(&next.stderr).contains("removed automatically once the program exits"));
        assert!(
            old.try_wait().unwrap().is_none(),
            "the fixture must still map the retired image"
        );
        (retired, setup)
    };
    let _ = old.kill();
    old.wait().unwrap();
    let (retired, setup) = result;
    assert_exit(&setup(), 0);
    assert!(fs::read_dir(retired).is_ok_and(|entries| entries.count() == 0));
}

#[cfg(windows)]
#[test]
fn windows_uninstall_reports_both_the_leftover_image_and_its_stage() {
    let fixture = SetupFixture::installed("ck-uninstall-mapped-image");
    let root = fixture.data_home.join("cortexkit");
    let public = root.join("cmd/ck.exe");
    fs::create_dir_all(public.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ck-under-test"), &public).unwrap();
    let public = fs::canonicalize(public).unwrap();
    let manifest = root.join("installer-manifest.json");
    fs::write(
        manifest,
        serde_json::to_vec(&json!({
            "schema_version": 1, "platform": "windows-x64",
            "mutations": [{"kind":"managed-binary", "path":public, "component":"core"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command_from(
            Command::new(&public),
            &index,
            &["setup", "--uninstall", "--yes"],
        )
        .env("CK_TEST_SETUP_RUNTIME_MISSING", "1")
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let report = text(&output.stdout);
    assert!(!public.exists());
    assert!(!root.join("installer.journal").exists());
    assert_eq!(
        fs::read_dir(root.join("installer.retired"))
            .unwrap()
            .count(),
        1,
        "cleanup of the mapped image must have a closed retired journal"
    );
    let stage = fs::read_dir(public.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .unwrap();
    let image = stage.join("previous");
    assert!(image.is_file());
    assert!(
        report
            .lines()
            .any(|line| line.contains("image:") && line.contains(&image.display().to_string())),
        "{report}"
    );
    assert!(
        report
            .lines()
            .any(|line| line.contains("staging directory:")
                && line.contains(&stage.display().to_string())),
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn setup_releases_its_lock_at_confirmation_and_refuses_a_stale_answer() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};

    let fixture = SetupFixture::installed("ck-confirmation-race");
    let path = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["modules"]
        .as_object_mut()
        .unwrap()
        .remove("subc-mcp");
    fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let interactive = fixture.command(&index, &["setup"]);
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 25,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(interactive.get_program());
    command.args(interactive.get_args());
    for (name, value) in interactive.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let (send, ready) = std::sync::mpsc::channel();
    let screen = std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut scratch = [0; 4096];
        while let Ok(count @ 1..) = reader.read(&mut scratch) {
            let prompted = text(&all).contains("Continue? [y/N]");
            all.extend_from_slice(&scratch[..count]);
            if !prompted && text(&all).contains("Continue? [y/N]") {
                let _ = send.send(());
            }
        }
        all
    });
    let mut first = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    if ready
        .recv_timeout(std::time::Duration::from_secs(10))
        .is_err()
    {
        first.kill().unwrap();
        panic!("setup did not ask for confirmation");
    }
    let mut second = fixture
        .command(&index, &["setup"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    if status.is_none() {
        first.kill().unwrap();
        second.kill().unwrap();
        let _ = first.wait();
        let _ = second.wait();
        panic!("the second setup could not finish while the first waited for input");
    }
    assert!(status.unwrap().success());
    let installed = fs::read(&path).unwrap();
    writer.write_all(b"yes\n").unwrap();
    writer.flush().unwrap();
    assert!(!first.wait().unwrap().success());
    drop(writer);
    drop(pty.master);
    assert!(text(&screen.join().unwrap()).contains("changed while waiting for confirmation"));
    assert_eq!(fs::read(path).unwrap(), installed);
}

#[cfg(windows)]
#[test]
fn setup_cleans_the_rollback_row_left_by_a_released_windows_upgrade() {
    let fixture = SetupFixture::installed("ck-released-windows-rollback");
    let root = fixture.data_home.join("cortexkit");
    let manifest = root.join("installer-manifest.json");
    let public = root.join("cmd/ck.exe");
    fs::create_dir_all(public.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ck-under-test"), &public).unwrap();
    let old = public.with_extension("exe.old");
    fs::write(&old, b"old released image").unwrap();
    let replaced = public.with_file_name(".ck.exe.replaced-105-1");
    fs::write(&replaced, b"another released image").unwrap();
    let unverified = public.with_file_name("not-a-released-backup.exe");
    fs::write(&unverified, b"unverifiable legacy row").unwrap();
    let mut document: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let rows = document["mutations"].as_array_mut().unwrap();
    rows.push(json!({"kind": "managed-binary", "path": public, "component": "core"}));
    rows.push(json!({"kind": "self-update-rollback", "path": old, "replaces": public}));
    rows.push(json!({"kind": "replaced-image", "path": replaced, "replaces": public}));
    rows.push(json!({"kind":"replaced-image", "path":unverified, "replaces":public}));
    fs::write(&manifest, serde_json::to_vec(&document).unwrap()).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let setup = || {
        fixture
            .command_from(Command::new(&public), &index, &["setup"])
            .output()
            .unwrap()
    };
    let output = setup();
    assert_exit(&output, 0);
    assert!(!old.exists());
    assert!(!replaced.exists());
    assert!(manifest_rows(&fixture, "self-update-rollback").is_empty());
    assert!(unverified.exists());
    assert_eq!(manifest_rows(&fixture, "replaced-image").len(), 1);
    assert!(text(&output.stderr)
        .contains("remove this file yourself; the installer will drop the row on the next run"));
    fs::remove_file(unverified).unwrap();
    assert_exit(&setup(), 0);
    assert!(manifest_rows(&fixture, "replaced-image").is_empty());
    assert!(public.is_file());
}

fn stop_at(fixture: &SetupFixture, index: &SignedIndex, point: &str) {
    kill_at(
        fixture.command(index, &["setup"]),
        &fixture._root.path().join(point),
        point,
    );
}

fn kill_at(mut command: Command, marker: &Path, point: &str) {
    // The installer's output goes to files, not pipes: nothing reads a pipe
    // while the installer runs, so a full pipe would block it before it reached
    // the crash point.
    let stdout_path = marker.with_extension("stdout");
    let stderr_path = marker.with_extension("stderr");
    let mut child = command
        .env("CK_TEST_INSTALL_CRASH_POINT", point)
        .env("CK_TEST_INSTALL_CRASH_MARKER", marker)
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("start installer command");
    // At the crash point the installer publishes the marker and then sleeps
    // until killed, so waiting for the marker or for the installer to exit is
    // enough. The bound only turns a hang into a clear failure; it is not a
    // timing assumption, so it is far above any loaded runner's time.
    let bound = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while !marker.exists() {
        if child.try_wait().unwrap().is_some() {
            panic!(
                "installer exited before reaching {point}: {} {}",
                fs::read_to_string(&stdout_path).unwrap_or_default(),
                fs::read_to_string(&stderr_path).unwrap_or_default()
            );
        }
        if std::time::Instant::now() > bound {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("installer did not reach {point} within 300 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(fs::read_to_string(marker).unwrap(), point);
}

#[test]
fn installer_recovers_a_killed_uninstall_after_the_public_ck_was_removed() {
    let fixture = SetupFixture::installed("ck-reinstall-after-killed-uninstall");
    let root = fixture.data_home.join("cortexkit");
    let public = root.join("cmd").join(platform_binary("ck"));
    fs::create_dir_all(public.parent().unwrap()).unwrap();
    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_ck-under-test")), &public);
    let old = fs::canonicalize(&public).unwrap();
    let digest = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(fs::read(&public).unwrap())
    );
    fs::write(root.join("installer-manifest.json"), serde_json::to_vec(&json!({
        "schema_version": 1, "platform": host_target(),
        "mutations": [{"kind":"managed-binary", "path":old, "component":"core", "sha256":digest}]
    })).unwrap()).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let mut uninstall = fixture.command_from(
        Command::new(&public),
        &index,
        &["setup", "--uninstall", "--yes"],
    );
    uninstall.env("CK_TEST_SETUP_RUNTIME_MISSING", "1");
    kill_at(
        uninstall,
        &fixture._root.path().join("uninstalled"),
        "file-act",
    );
    assert!(!public.exists());
    let temp = fixture._root.path().join("verified-temp");
    fs::create_dir(&temp).unwrap();
    let stage = temp.join("release");
    subc_os::create_private_directory_exclusive(&stage).unwrap();
    let extracted = stage.join(platform_binary("ck"));
    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_ck")), &extracted);
    let output = fixture
        .command_from(
            Command::new(&extracted),
            &index,
            &["__install-bootstrap", &"a".repeat(64), "9.8.7"],
        )
        .env("TMPDIR", &temp)
        .env("TMP", &temp)
        .env("TEMP", &temp)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert!(text(&output.stdout).contains("rolled back"));
    assert!(text(&output.stdout).contains("Placed ck at"));
    assert!(fs::read(public).unwrap() == fs::read(extracted).unwrap());
}

#[test]
fn installer_recovers_an_unrelated_open_journal_when_cmd_ck_is_missing() {
    let fixture = SetupFixture::installed("restart-with-missing-command");
    let public = fixture
        .data_home
        .join("cortexkit/cmd")
        .join(platform_binary("ck"));
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut document: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    document["modules"]
        .as_object_mut()
        .unwrap()
        .remove("subc-mcp");
    let before = serde_json::to_vec_pretty(&document).unwrap();
    fs::write(&config, &before).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    stop_at(&fixture, &index, "file-act");
    assert_ne!(fs::read(&config).unwrap(), before);
    assert!(!public.exists());

    let temp = fixture._root.path().join("verified-temp");
    fs::create_dir(&temp).unwrap();
    let stage = temp.join("release");
    subc_os::create_private_directory_exclusive(&stage).unwrap();
    let extracted = stage.join(platform_binary("ck"));
    common::copy_executable(Path::new(env!("CARGO_BIN_EXE_ck")), &extracted);
    let output = fixture
        .command_from(
            Command::new(&extracted),
            &index,
            &["__install-bootstrap", &"a".repeat(64), "9.8.7"],
        )
        .env("TMPDIR", &temp)
        .env("TMP", &temp)
        .env("TEMP", &temp)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert!(text(&output.stdout).contains("rolled back"));
    assert!(text(&output.stdout).contains("Placed ck at"));
    assert_eq!(fs::read(config).unwrap(), before);
    assert!(fs::read(public).unwrap() == fs::read(extracted).unwrap());
}

#[test]
fn a_killed_upgrade_replays_the_previous_config_before_a_failed_index_fetch() {
    let fixture = UpgradeFixture::new("ck-upgrade-config-crash");
    let config = fixture.home.join(".config/cortexkit/subc.jsonc");
    let before = b"{\"modules\":{}}\n";
    fs::write(&config, before).unwrap();
    let root = fixture.home.join(".local/share/cortexkit");
    let manifest = root.join("installer-manifest.json");
    let old_inventory = fs::read(&manifest).unwrap();
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.1",
            &"22".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    kill_at(
        fixture.command(&index, &["upgrade"]),
        &fixture._root.path().join("upgrade-act"),
        "file-act",
    );
    assert_ne!(fs::read(&config).unwrap(), before);
    assert_eq!(fs::read(&manifest).unwrap(), old_inventory);
    let output = fixture
        .command(&index, &["upgrade"])
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/missing")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(text(&output.stdout).contains("Recovered install transaction"));
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_eq!(fs::read(&manifest).unwrap(), old_inventory);
    assert!(!root.join("installer.journal").exists());
}

#[test]
fn a_killed_setup_resumes_config_undo_before_replanning() {
    let fixture = SetupFixture::installed("ck-journal-config-crash");
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut parsed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    parsed["modules"]
        .as_object_mut()
        .unwrap()
        .remove("subc-mcp");
    let before = serde_json::to_vec_pretty(&parsed).unwrap();
    fs::write(&config, &before).unwrap();
    let root = fixture.data_home.join("cortexkit");
    let manifest = root.join("installer-manifest.json");
    let previous_ownership = fs::read(&manifest).unwrap();
    let journal = root.join("installer.journal");
    let index = serve_signed_index(|base| setup_index(base, None));

    stop_at(&fixture, &index, "file-write-ahead");
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(journal.exists());
    stop_at(&fixture, &index, "file-act");
    assert_ne!(fs::read(&config).unwrap(), before);
    assert_eq!(fs::read(&manifest).unwrap(), previous_ownership);
    let pending_bytes = fs::read(&journal).unwrap();
    let preview = fixture
        .command(&index, &["setup", "--dry-run"])
        .output()
        .unwrap();
    assert!(text(&preview.stdout).contains("unfinished install transaction"));
    assert_eq!(fs::read(&journal).unwrap(), pending_bytes);
    stop_at(&fixture, &index, "file-undo");
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_eq!(fs::read(&manifest).unwrap(), previous_ownership);

    let completed = fixture.command(&index, &["setup"]).output().unwrap();
    assert_exit(&completed, 0);
    assert!(!journal.exists());
    assert!(fs::read_to_string(config).unwrap().contains("subc-mcp"));
}

#[cfg(unix)]
#[test]
fn a_kill_after_core_commit_does_not_skip_bus_monitoring_on_the_next_setup() {
    let fixture = SetupFixture::installed("ck-journal-bus-crash");
    let nats = fixture._root.path().join("nats");
    fs::create_dir(&nats).unwrap();
    let server = nats.join("server.conf");
    fs::write(&server, "unchanged operator settings\n").unwrap();
    let ckbus = fixture._root.path().join("mock-ck-bus");
    write_executable(
        &ckbus,
        r#"#!/bin/sh
if [ "$6" = --dry-run ]; then
  if grep -q '^http_port' "$5/server.conf"; then status=unchanged; else status='would apply'; fi
else
  if grep -q '^http_port' "$5/server.conf"; then status=unchanged; else
    printf 'http_port: 8222\n' >>"$5/server.conf"
    status=applied
  fi
fi
printf '{"status":"%s","health_url":"http://127.0.0.1:8222/healthz"}\n' "$status"
"#,
    );
    let manifest = fixture.data_home.join("cortexkit/installer-manifest.json");
    let mut inventory: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let mutations = inventory["mutations"].as_array_mut().unwrap();
    mutations.retain(|row| row["kind"] != "bus-trust");
    mutations.push(json!({"kind": "managed-binary", "path": ckbus, "component": "core"}));
    fs::write(manifest, serde_json::to_vec_pretty(&inventory).unwrap()).unwrap();
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut document: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    document["modules"]["ckbus"] = json!({ "program": ckbus });
    document["modules"]["nats-server"] = json!({ "program": fixture.data_home.join("cortexkit/bin/ck-nats-server"), "args": ["-c", server], "protocol": "none" });
    fs::write(&config, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    stop_at(&fixture, &index, "before-bus-monitoring");
    assert_eq!(
        fs::read_to_string(&server).unwrap(),
        "unchanged operator settings\n"
    );
    assert!(!fixture
        .data_home
        .join("cortexkit/installer.journal")
        .exists());
    let committed = subc_jsonc::read_document(&config).unwrap().1;
    assert!(committed["modules"]["nats-server"]["health"].is_null());
    assert_exit(&fixture.command(&index, &["setup"]).output().unwrap(), 0);
    let updated = fs::read_to_string(&server).unwrap();
    assert!(updated.contains("http_port: 8222"));
    let config_after = fs::read(&config).unwrap();
    assert!(String::from_utf8_lossy(&config_after).contains("http://127.0.0.1:8222/healthz"));
    assert_exit(&fixture.command(&index, &["setup"]).output().unwrap(), 0);
    assert_eq!(fs::read_to_string(server).unwrap(), updated);
    assert_eq!(fs::read(config).unwrap(), config_after);
}
