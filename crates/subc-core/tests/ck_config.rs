use serde_json::{json, Value};
mod common;
use common::kit::Kit;

#[test]
fn ck_config_commands_reach_the_daemon_and_status_retains_all_diagnostics() {
    let kit = Kit::boot_with_config_gate("ck-config", &[]);
    let output = kit.ck(&["--json", "config", "status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["op"], "config.status");
    assert_eq!(body["status"]["state"], "unstamped");
    for field in [
        "ungated_modules",
        "ungated_inputs",
        "gated_modules",
        "interpreter_only_covered",
        "approved_but_inactive",
        "development_switches",
        "missing_approval_surfaces",
        "warnings",
        "integrity",
        "anchor_pending",
        "pending_proposals",
    ] {
        assert!(
            body["status"].get(field).is_some(),
            "missing {field}: {body}"
        );
    }
    assert!(body["status"]["ungated_inputs"]
        .as_array()
        .unwrap()
        .contains(&json!("SUBC_PORT")));
    for args in [vec!["config", "stamp"], vec!["config", "history"]] {
        let output = kit.ck(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["op"], format!("config.{}", args[1]));
    }
    for args in [
        vec!["config", "plan"],
        vec!["config", "unstamp"],
        vec!["config", "rollback", "999"],
        vec![
            "config",
            "apply",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ],
    ] {
        let output = kit.ck(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            !stderr.contains("usage:"),
            "{args:?} did not parse: {stderr}"
        );
        assert!(
            !stderr.contains("unsupported_control"),
            "{args:?} did not dispatch: {stderr}"
        );
    }
    let output = kit.ck(&[
        "config",
        "place-program",
        "demo",
        "/not/read/while/unstamped",
        &"a".repeat(64),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["op"],
        "config.place_program"
    );
}

#[test]
fn ck_config_poisoned_environment_connects_only_to_rig_daemon() {
    const CHILD: &str = "CK_CONFIG_POISON_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let kit = Kit::boot_with_config_gate("ck-config-poison-child", &[]);
        assert_eq!(kit.answering_pid(), kit.pid());
        let output = kit.ck(&["config", "status"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let kit = Kit::boot("ck-config-poison-parent", &[]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let home = kit.root().join("decoy-home");
    let decoy = home.join(".local/share/cortexkit/run/subc-connection.json");
    std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();
    let mut info: Value =
        serde_json::from_slice(&std::fs::read(kit.connection()).unwrap()).unwrap();
    info.as_object_mut().unwrap().remove("local_endpoint");
    info["endpoints"] = json!([{"host":"127.0.0.1","port":listener.local_addr().unwrap().port()}]);
    std::fs::write(&decoy, serde_json::to_vec(&info).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ck_config_poisoned_environment_connects_only_to_rig_daemon",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SUBC_CONNECTION_FILE", &decoy)
        .output()
        .unwrap();
    let attempt = listener.accept();
    assert!(
        matches!(attempt, Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "a connection to the live decoy was attempted"
    );
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ck_config_missing_or_failed_local_endpoint_never_tries_tcp() {
    let kit = Kit::boot_with_config_gate("ck-config-no-local", &[]);
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    tcp.set_nonblocking(true).unwrap();
    let mut info: Value =
        serde_json::from_slice(&std::fs::read(kit.connection()).unwrap()).unwrap();
    info["endpoints"] = json!([{"host":"127.0.0.1","port":tcp.local_addr().unwrap().port()}]);
    info.as_object_mut().unwrap().remove("local_endpoint");
    let path = kit.root().join("no-local.json");
    for missing in [true, false] {
        if !missing {
            #[cfg(unix)]
            {
                info["local_endpoint"] =
                    json!({"kind":"unix", "path":kit.root().join("not-a-socket")});
            }
            #[cfg(windows)]
            {
                info["local_endpoint"] = json!({"kind":"pipe", "name":format!("\\\\.\\pipe\\ck-config-absent-{}", std::process::id())});
            }
        }
        std::fs::write(&path, serde_json::to_vec(&info).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let output = kit
            .ck_command()
            .arg("--subc")
            .arg(&path)
            .args(["config", "status"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains(if missing {
                "daemon published no local endpoint"
            } else {
                "local connect"
            }),
            "{stderr}"
        );
        assert!(
            stderr.contains("check `ck daemon` and the daemon log, then retry"),
            "{stderr}"
        );
        assert!(
            matches!(tcp.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "config attempted TCP fallback"
        );
    }
}
