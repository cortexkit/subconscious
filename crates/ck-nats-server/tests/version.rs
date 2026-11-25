#[test]
fn version_exits_before_config_engine_or_daemon_access() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ck-nats-server"))
        .args(["--version", "--unknown"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        output.stdout,
        format!("ck-nats-server {}\n", env!("CARGO_PKG_VERSION")).into_bytes()
    );
    assert!(output.stderr.is_empty());
}
