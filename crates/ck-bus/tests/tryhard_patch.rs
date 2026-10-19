use serde_json::Value;
use std::process::Command;

const COMMONS_REVISION: &str = "46853185cd2ee6b6bcb684ca7d63663d59d6c8cb";

fn cargo_output(args: &[&str]) -> std::process::Output {
    Command::new("cargo")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo")
}

#[test]
fn tryhard_resolves_from_the_pinned_commons_commit() {
    let output = cargo_output(&["metadata", "--locked", "--format-version", "1"]);
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata JSON");
    let tryhard: Vec<_> = metadata["packages"]
        .as_array()
        .expect("metadata packages")
        .iter()
        .filter(|package| package["name"] == "tryhard")
        .collect();
    assert_eq!(
        tryhard.len(),
        1,
        "metadata must resolve one tryhard package"
    );
    assert_eq!(tryhard[0]["version"], "0.5.2");
    let source = tryhard[0]["source"]
        .as_str()
        .expect("patched tryhard has a source");
    assert!(
        source.starts_with("git+https://github.com/cortexkit/commons?")
            && source.contains(COMMONS_REVISION),
        "tryhard must resolve from cortexkit/commons at {COMMONS_REVISION}, got {source}"
    );
}

#[test]
fn ck_bus_normal_graph_does_not_enable_tokio_test_util() {
    let output = cargo_output(&[
        "tree",
        "--locked",
        "-p",
        "ck-bus",
        "--edges",
        "normal,features",
        "--invert",
        "tokio",
    ]);
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        !tree.contains("tokio feature \"test-util\""),
        "ck-bus's normal dependency graph must not enable tokio test-util:\n{tree}"
    );
}
