use std::{fs, process::Command};
use subc_test_support::TestTempDir;

#[test]
fn manifest_is_emitted_offline_without_module_setup() {
    let output = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"))
        .env_clear()
        .arg("--manifest")
        .output()
        .expect("subc MCP manifest binary starts");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("manifest JSON");
    assert_eq!(manifest["runtime_computed"], serde_json::json!([]));
    assert!(manifest.get("provenance").is_none());
    assert_eq!(manifest["module_id"], "ck-subc-mcp");
    // The version is the crate's own, so a release bump never needs this test
    // edited; everything else is compared whole against the captured
    // pre-logger manifest. Compared as parsed values, independent of
    // serde_json's preserve_order feature, which other crates enable in
    // workspace builds.
    assert_eq!(manifest["module_version"], env!("CARGO_PKG_VERSION"));
    let mut rest = manifest.clone();
    rest.as_object_mut().unwrap().remove("module_version");
    let baseline = "{\"consumes\":[{\"of\":[],\"role\":\"tool_client\"}],\"module_id\":\"ck-subc-mcp\",\"protocol_ver\":2,\"provides\":[],\"runtime_computed\":[]}";
    assert_eq!(
        rest,
        serde_json::from_str::<serde_json::Value>(baseline).unwrap()
    );
}

#[test]
fn module_startup_writes_dated_r2_segment() {
    let home = TestTempDir::new("subc-mcp-log");
    let output = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"))
        .env("XDG_DATA_HOME", home.path())
        .env("CK_LOG", "info")
        .env("SUBC_MODULE_ID", "ck-subc-mcp")
        .env("SUBC_LAUNCH_NONCE", "test-nonce")
        .arg("module")
        .arg("--subc")
        .arg(home.join("missing-connection.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let logs = home.join("cortexkit/ck-subc-mcp/logs");
    let entries: Vec<_> = fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let name = entries[0].file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with("ck-subc-mcp.20") && name.ends_with(".log"),
        "{name}"
    );
    let date = name
        .strip_prefix("ck-subc-mcp.")
        .unwrap()
        .strip_suffix(".log")
        .unwrap();
    assert_eq!(date.len(), 10);
    assert!(date.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 {
        c == '-'
    } else {
        c.is_ascii_digit()
    }));
    let line = fs::read_to_string(&entries[0]).unwrap();
    assert!(
        line.lines()
            .any(|line| line.contains(" INFO  ck-subc-mcp: module starting")
                && line.ends_with("module starting")),
        "{line}"
    );
    assert!(
        line.lines()
            .all(|line| line.contains('T') && line.contains('Z')),
        "{line}"
    );
}

#[test]
fn shim_logs_without_daemon_environment_or_protocol_stdout() {
    let home = TestTempDir::new("subc-mcp-shim-log");
    let output = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"))
        .env_remove("SUBC_MODULE_ID")
        .env("XDG_DATA_HOME", home.path())
        .env("CK_LOG", "info")
        .args(["shim", "--module-connection-file"])
        .arg(home.join("missing-connection.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "shim must reserve stdout for MCP");
    assert!(
        output.stderr.is_empty(),
        "shim diagnostics must not reach host stderr"
    );
    let entries: Vec<_> = fs::read_dir(home.join("cortexkit/ck-subc-mcp/logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    assert!(fs::read_to_string(&entries[0])
        .unwrap()
        .contains("ck-subc-mcp.shim: [harness=mcp:generic] shim starting"));
}
