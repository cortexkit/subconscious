use std::{fs, process::Command};
use subc_test_support::TestTempDir;

#[test]
fn manifest_is_emitted_offline_before_startup_attestation() {
    let output = Command::new(env!("CARGO_BIN_EXE_ck-mcp-stdio-adapter"))
        .env_clear()
        .arg("--manifest")
        .output()
        .expect("adapter manifest binary starts");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("manifest JSON");
    assert_eq!(manifest["runtime_computed"], serde_json::json!([]));
    assert_eq!(manifest["module_id"], "mcp-stdio-adapter");
    // Other workspace crates can enable serde_json's preserve_order feature;
    // compare the pre-logger manifest independent of map serialization order.
    let normalized = String::from_utf8(output.stdout).unwrap().replace(
        &format!("\"module_version\":\"{}\"", env!("CARGO_PKG_VERSION")),
        "\"module_version\":\"0.1.0\"",
    );
    let baseline = "{\"module_id\":\"mcp-stdio-adapter\",\"module_version\":\"0.1.0\",\"protocol_ver\":2,\"provides\":[{\"concurrency\":\"module_managed\",\"config_schema\":{\"type\":\"object\"},\"identity_scope\":[\"project\",\"session\"],\"observability\":[{\"kind\":\"snapshot\",\"name\":\"health\"}],\"operations\":[{\"description\":\"List the MCP tools exposed by the configured child servers and return their names, descriptions, and input schemas.\",\"kind\":\"query\",\"name\":\"tools/list\"},{\"description\":\"Invoke a named tool on a configured child server and return the child's MCP result.\",\"kind\":\"mutate\",\"name\":\"tools/call\"}],\"role\":\"management_surface\"}],\"runtime_computed\":[]}";
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&normalized).unwrap(),
        serde_json::from_str::<serde_json::Value>(baseline).unwrap()
    );
}

#[test]
fn unattested_binary_exits_before_any_startup_connection_work() {
    let output = Command::new(env!("CARGO_BIN_EXE_ck-mcp-stdio-adapter"))
        .env_clear()
        .output()
        .expect("adapter binary starts");

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        "ck-mcp-stdio-adapter: startup attestation requires SUBC_MODULE_ID\n"
    );
}

#[test]
fn adapter_startup_writes_dated_r2_segment() {
    let home = TestTempDir::new("mcp-adapter-log");
    let output = Command::new(env!("CARGO_BIN_EXE_ck-mcp-stdio-adapter"))
        .env("XDG_DATA_HOME", home.path())
        .env("CK_LOG", "info")
        .env("SUBC_MODULE_ID", "mcp-stdio-adapter")
        .env("SUBC_LAUNCH_NONCE", "test-nonce")
        .arg("--config")
        .arg(home.join("missing-config.jsonc"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let logs = home.join("cortexkit/mcp-stdio-adapter/logs");
    let entries: Vec<_> = fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let name = entries[0].file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with("mcp-stdio-adapter.20") && name.ends_with(".log"),
        "{name}"
    );
    let date = name
        .strip_prefix("mcp-stdio-adapter.")
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
        line.lines().any(
            |line| line.contains(" INFO  mcp-stdio-adapter: adapter starting")
                && line.ends_with("adapter starting")
        ),
        "{line}"
    );
    assert!(
        line.lines()
            .all(|line| line.contains('T') && line.contains('Z')),
        "{line}"
    );
}
