use std::process::Command;

#[test]
fn version_exits_without_supervision_or_broker_config() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ck-bus"))
        .arg("--version")
        .env_clear()
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_RUNTIME_DIR", home.path().join("run"))
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        output.stdout,
        format!("ck-bus {}\n", env!("CARGO_PKG_VERSION")).into_bytes()
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}
