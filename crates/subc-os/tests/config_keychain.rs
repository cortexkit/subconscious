#![cfg(feature = "test-support")]

use std::process::{Command, Output};

#[test]
fn config_keychain_release_graphs_exclude_test_seams() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // These are the published binary targets in release.yml and the Darwin
    // release script. Tree resolution needs no linker or target standard library.
    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "aarch64-apple-darwin",
    ] {
        let output = Command::new(env!("CARGO"))
            .current_dir(&workspace)
            .args([
                "tree",
                "-e",
                "normal,build,features",
                "--locked",
                "--target",
                target,
                "-p",
                "subc-core",
                "-p",
                "subc-mcp",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let graph = String::from_utf8(output.stdout).unwrap();
        assert!(
            graph.contains("subc-os v"),
            "empty or incomplete {target} graph"
        );
        for feature in ["test-support", "dev-placement", "dev-desktop-approval"] {
            assert!(
                !graph.contains(&format!("feature \"{feature}\"")),
                "{target} enables {feature}"
            );
        }
        println!("{target}: release graph excludes test/development features");
    }
    let manifest = include_str!("../Cargo.toml");
    let fixture = manifest
        .split("[[bin]]")
        .find(|entry| entry.contains("name = \"ckdev-os-keychain\""))
        .expect("keychain fixture is declared");
    assert!(fixture.contains("required-features = [\"test-support\"]"));
}

#[test]
fn config_keychain_fixture_uses_file_store_and_refuses_renamed_binary() {
    let root = cortexkit_test_support::ScratchDir::new("keychain-fixture");
    let path = root.join("item");
    let fixture = std::path::Path::new(env!("CARGO_BIN_EXE_ckdev-os-keychain"));
    let invoke = |binary: &std::path::Path, args: &[&str]| -> Output {
        Command::new(binary)
            .arg("--file")
            .arg(&path)
            .args(args)
            .env("HOME", &*root)
            .env("XDG_DATA_HOME", &*root)
            .env("XDG_CONFIG_HOME", &*root)
            .env("XDG_RUNTIME_DIR", &*root)
            .env("SUBC_CONNECTION_FILE", root.join("connection.json"))
            .output()
            .unwrap()
    };
    let read = invoke(fixture, &["read"]);
    assert!(read.status.success(), "{read:?}");
    assert_eq!(read.stdout, b"missing\n");
    let renamed = root.join(if cfg!(windows) {
        "release-keychain.exe"
    } else {
        "release-keychain"
    });
    std::fs::copy(fixture, &renamed).unwrap();
    let refused = invoke(&renamed, &["create"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(!path.exists());
    let created = invoke(fixture, &["create"]);
    assert!(created.status.success(), "{created:?}");
    let digest = "51".repeat(32);
    let anchored = invoke(fixture, &["anchor", "1", &digest]);
    assert!(anchored.status.success(), "{anchored:?}");
    let denied = invoke(fixture, &["--deny", "read"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("key access denied"));
    let failed = invoke(fixture, &["--fail-updates", "clear", "2"]);
    assert!(!failed.status.success());
    let unchanged = invoke(fixture, &["read"]);
    assert!(unchanged.status.success());
    assert_eq!(
        String::from_utf8(unchanged.stdout).unwrap(),
        format!("high_water=1\ncurrent=1 sha256={digest}\n")
    );
    let cleared = invoke(fixture, &["clear", "2"]);
    assert!(cleared.status.success(), "{cleared:?}");
    let deleted = invoke(fixture, &["delete"]);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(!path.exists());
}
