use std::process::Command;

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
