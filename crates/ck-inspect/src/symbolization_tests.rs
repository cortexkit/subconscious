use super::atos_with;
use crate::profile::Profile;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const UUID: &str = "D9F36BE8D4E23E7AA29C8B8BA9E6219A";

fn dwarf(root: &Path, bundle: &str, file: &str) -> PathBuf {
    let path = root
        .join(UUID)
        .join(bundle)
        .join("Contents/Resources/DWARF")
        .join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"debug info").unwrap();
    path
}

fn script(root: &Path, contents: &str) -> PathBuf {
    let path = root.join("atos");
    super::write_executable(&path, &format!("#!/bin/sh\n{contents}\n"));
    path
}

fn sample(offsets: &[u64]) -> crate::sample::Sample {
    let mut graph = format!("Call graph:\n  {} Thread_1\n", offsets.len());
    for offset in offsets {
        graph.push_str(&format!(
            "    1 ???  (in helper)  load address 0x1000 + 0x{offset:x}  [0x0]\n"
        ));
    }
    graph.push_str(&format!(
        "Binary Images:\n0x1000 - 0x2000 helper (1) <{UUID}> /helper\n"
    ));
    crate::sample::parse(
        &graph,
        Profile::new("fixture".into(), 1, 1, "sample".into()),
    )
    .unwrap()
}

async fn run(root: &Path, atos: &Path, offsets: &[u64]) -> Value {
    let mut sample = sample(offsets);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    sample
        .symbolize_with(root, |file, load, addresses| {
            atos_with(atos, file, load, addresses, deadline)
        })
        .await;
    serde_json::to_value(sample.finish()).unwrap()
}

fn assert_outcome(
    profile: &Value,
    status: &str,
    reason: Option<&str>,
    resolved: usize,
    unresolved: usize,
) {
    let mut expected = json!({"image":"helper", "uuid":UUID, "status":status, "resolved":resolved, "unresolved":unresolved});
    if let Some(reason) = reason {
        expected["reason"] = reason.into();
    }
    assert_eq!(profile.get("symbolization"), Some(&json!([expected])));
}

#[tokio::test]
async fn symbolization_complete_counts_distinct_frames() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let atos = script(root.path(), "printf 'named (in helper)\\n'");
    let profile = run(root.path(), &atos, &[0x10, 0x10]).await;
    assert_outcome(&profile, "complete", None, 1, 0);
    assert_eq!(profile["threads"][0]["stacks"][0]["frames"][0], "named");
}

#[tokio::test]
async fn symbolization_partial_reports_unresolved() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let atos = script(root.path(), "printf 'named\\n0x1020\\n'");
    let profile = run(root.path(), &atos, &[0x10, 0x20]).await;
    assert_outcome(&profile, "partial", Some("unresolved"), 1, 1);
}

#[tokio::test]
async fn symbolization_none_reports_unresolved() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    // Empty, unknown, echoed and missing lines all leave their addresses unnamed.
    let atos = script(root.path(), "printf '\\n???\\n0x1030\\n'");
    let profile = run(root.path(), &atos, &[0x10, 0x20, 0x30, 0x40]).await;
    assert_outcome(&profile, "none", Some("unresolved"), 0, 4);
}

#[tokio::test]
async fn symbolization_debug_file_missing() {
    for layout in 0..4 {
        let root = tempfile::tempdir().unwrap();
        match layout {
            1 => std::fs::create_dir(root.path().join(UUID)).unwrap(),
            2 => {
                let file = dwarf(root.path(), "helper.dSYM", "debug");
                std::fs::remove_file(file).unwrap();
            }
            3 => {
                std::fs::write(root.path().join(UUID), b"not a directory").unwrap();
            }
            _ => {}
        }
        let profile = run(root.path(), Path::new("/no/symbolizer"), &[0x10]).await;
        assert_outcome(&profile, "none", Some("debug_file_missing"), 0, 1);
    }
}

#[tokio::test]
async fn symbolization_debug_file_ambiguous() {
    for second_bundle in [false, true] {
        let root = tempfile::tempdir().unwrap();
        dwarf(root.path(), "helper.dSYM", "debug");
        dwarf(
            root.path(),
            if second_bundle {
                "second.dSYM"
            } else {
                "helper.dSYM"
            },
            "second",
        );
        let profile = run(root.path(), Path::new("/no/symbolizer"), &[0x10]).await;
        assert_outcome(&profile, "none", Some("debug_file_ambiguous"), 0, 1);
    }
}

#[tokio::test]
async fn symbolization_symbolizer_missing() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let profile = run(root.path(), &root.path().join("missing-atos"), &[0x10]).await;
    assert_outcome(&profile, "none", Some("symbolizer_missing"), 0, 1);
}

#[tokio::test]
async fn symbolization_symbolizer_failed() {
    for contents in ["exit 1", "printf '\\377'", "printf 'named\\n'; exit 1"] {
        let root = tempfile::tempdir().unwrap();
        dwarf(root.path(), "helper.dSYM", "debug");
        let atos = script(root.path(), contents);
        let profile = run(root.path(), &atos, &[0x10]).await;
        assert_outcome(&profile, "none", Some("symbolizer_failed"), 0, 1);
        assert_eq!(
            profile["threads"][0]["stacks"][0]["frames"][0],
            "helper+0x10"
        );
    }
}

#[tokio::test]
async fn symbolization_address_cap() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let atos = script(root.path(), "shift 4; test \"$#\" -eq 256 || exit 1; test \"$1\" = 0x1000 || exit 1; for address do printf 'named\\n'; done; test \"$address\" = 0x10ff");
    let profile = run(root.path(), &atos, &(0..300).collect::<Vec<_>>()).await;
    assert_outcome(&profile, "partial", Some("address_cap"), 256, 44);
}

#[tokio::test]
async fn symbolization_pipeline_reason_precedence() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let offsets = (0..300).collect::<Vec<_>>();
    let atos = script(root.path(), "exit 1");
    assert_outcome(
        &run(root.path(), &atos, &offsets).await,
        "none",
        Some("symbolizer_failed"),
        0,
        300,
    );
    let atos = script(
        root.path(),
        "printf '???\\n'; shift 4; shift; for address do printf 'named\\n'; done",
    );
    assert_outcome(
        &run(root.path(), &atos, &offsets).await,
        "partial",
        Some("unresolved"),
        255,
        45,
    );
}

#[tokio::test(start_paused = true)]
async fn symbolization_deadline_before_start() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let atos = script(
        root.path(),
        &format!(
            "touch '{}'; exec sleep 600",
            root.path().join("started").display()
        ),
    );
    let mut sample = sample(&[0x10]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    tokio::time::advance(Duration::from_secs(1)).await;
    sample
        .symbolize_with(root.path(), |file, load, addresses| {
            atos_with(&atos, file, load, addresses, deadline)
        })
        .await;
    assert_outcome(
        &serde_json::to_value(sample.finish()).unwrap(),
        "none",
        Some("deadline"),
        0,
        1,
    );
    assert!(!root.path().join("started").exists());
}

#[tokio::test(start_paused = true)]
async fn symbolization_deadline_mid_run() {
    let root = tempfile::tempdir().unwrap();
    dwarf(root.path(), "helper.dSYM", "debug");
    let marker = root.path().join("started");
    let atos = script(
        root.path(),
        &format!("echo $$ > '{}'; exec sleep 600", marker.display()),
    );
    let dir = root.path().to_path_buf();
    let task = tokio::spawn(async move {
        let mut sample = sample(&[0x10]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        sample
            .symbolize_with(&dir, |file, load, addresses| {
                atos_with(&atos, file, load, addresses, deadline)
            })
            .await;
        serde_json::to_value(sample.finish()).unwrap()
    });
    let wall_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(!task.is_finished(), "symbolizer exited before starting");
        assert!(
            std::time::Instant::now() < wall_deadline,
            "symbolizer did not start"
        );
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_outcome(&task.await.unwrap(), "none", Some("deadline"), 0, 1);
}
