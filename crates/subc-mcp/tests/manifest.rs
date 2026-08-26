use std::process::Command;

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
}
