#[test]
fn version_exits_before_config_engine_or_daemon_access() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ck-nats-server"))
        .args(["--version", "--unknown"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"ck-nats-server 0.1.2\n");
    assert!(output.stderr.is_empty());
}
