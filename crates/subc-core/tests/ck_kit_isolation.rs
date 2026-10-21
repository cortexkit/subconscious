//! The private-daemon kit (`common::kit`) keeps `ck` away from every daemon
//! but its own, on every platform.
//!
//! Connection-file discovery's second and third rungs are built from HOME and
//! the temp directory, not from the XDG folders, so a `ck` given only private
//! XDG folders can still reach the operator's daemon. The kit gives each `ck`
//! its own HOME and temp directory too; these tests prove that with no
//! `--subc` it finds no daemon anywhere outside the test, that with `--subc`
//! the daemon answering is the kit's, and that `ck --json daemon` prints
//! every capability the daemon advertises.

use std::path::Path;

use serde_json::Value;
use subc_client_rs::{ConsumerOptions, SubcConsumer};

mod common;
use common::kit::Kit;

/// The paths a discovery failure reports trying, from `ck`'s one-line
/// stderr: `no usable subc connection file found; tried: <path> (<reason>),
/// <path> (<reason>), …`.
fn tried_paths(stderr: &str) -> Vec<String> {
    let line = stderr.lines().next().unwrap_or_default();
    let tried = line
        .split_once("tried: ")
        .unwrap_or_else(|| panic!("no tried list in ck's stderr: {stderr}"))
        .1;
    tried
        .split("), ")
        .map(|candidate| {
            candidate
                .rsplit_once(" (")
                .unwrap_or_else(|| panic!("unexpected candidate {candidate:?} in {stderr}"))
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn ck_in_the_kit_reaches_only_the_kit_daemon() {
    let kit = Kit::boot("ck-kit-isolation", &[]);

    // Without --subc, ck looks only inside the test's own folders and finds
    // nothing there.
    let output = kit
        .ck_command()
        .args(["--json", "module", "list"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "ck without --subc found a daemon:\n{}{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stdout.is_empty());
    assert!(
        stderr.contains("no usable subc connection file found"),
        "{stderr}"
    );
    let tried = tried_paths(&stderr);
    assert_eq!(
        tried.len(),
        3,
        "expected the runtime, home and temp candidates: {tried:?}"
    );
    let client_root = kit.client_root();
    let outside: Vec<&String> = tried
        .iter()
        .filter(|path| !Path::new(path).starts_with(&client_root))
        .collect();
    assert!(
        outside.is_empty(),
        "ck looked for a daemon outside the test's folders ({}): {outside:?}",
        client_root.display()
    );

    // With --subc, the daemon answering is the one the kit started.
    assert_eq!(kit.answering_pid(), kit.pid());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ck_json_daemon_prints_every_capability_the_daemon_advertises() {
    let kit = tokio::task::spawn_blocking(|| Kit::boot("ck-kit-capabilities", &[]))
        .await
        .unwrap();
    let consumer = SubcConsumer::connect(&kit.connection(), ConsumerOptions::default())
        .await
        .unwrap();
    let advertised = consumer.server_describe().await.unwrap().capabilities;
    consumer.close().await;
    assert!(
        !advertised.is_empty(),
        "the daemon advertises no capability"
    );

    let output = tokio::task::spawn_blocking(move || kit.ck(&["--json", "daemon"]))
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let printed: Vec<&str> = printed["capabilities"]
        .as_array()
        .expect("ck --json daemon prints a capabilities list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let missing: Vec<&String> = advertised
        .iter()
        .filter(|capability| !printed.contains(&capability.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "ck --json daemon left out advertised capabilities {missing:?}; it printed {printed:?}"
    );
}
