use std::process::Command;

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
