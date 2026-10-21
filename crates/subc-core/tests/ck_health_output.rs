//! `ck health <id>` prints exactly what it printed before its headline rule
//! and value wording moved into `ck_ui::metrics`: the default view (headline
//! metrics) and `--verbose` (the whole tree), for one module that names a
//! headline and one that doesn't. The expected text was captured from the
//! code as it was before the move.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cortexkit_test_support::{dev_command, ScratchDir};
use serde_json::{json, Value};

mod common;

struct Daemon {
    root: ScratchDir,
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for id in ["hl-alpha", "hl-beta"] {
            if let Some(pid) = fs::read_to_string(self.root.join(format!("{id}.pid")))
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

impl Daemon {
    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn ck(&self, args: &[&str]) -> String {
        let mut command = dev_command(common::ckdev_ck());
        common::kit::isolate_client(&mut command, &self.root.join("client"));
        let out = command
            .arg("--subc")
            .arg(self.connection())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ck {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

/// A module that names its headline, with values of every kind the wording
/// rule treats differently: counts, bytes, ages, small millisecond values,
/// booleans, a small object, nested lists and an empty list.
fn alpha_metrics() -> Value {
    json!({
        "headline": ["runs_today", "rss_bytes", "catalog_age_ms", "lanes"],
        "runs_today": 1284,
        "rss_bytes": 75264,
        "catalog_age_ms": 120000,
        "lanes": { "fast": 2, "slow": 1 },
        "wait_ms": 250,
        "enabled": true,
        "roots": [{ "id": "r1", "count": 3 }, { "id": "r2", "count": 0 }],
        "empty": []
    })
}

/// A module with no headline: its top-level scalars are shown, a null reads
/// "unknown", and the nested object only appears with `--verbose`.
fn beta_metrics() -> Value {
    json!({
        "queue": 3,
        "label": "steady",
        "missing": null,
        "idle_age_secs": 90,
        "empty_text": "",
        "sub": { "a": 1, "b": "two" }
    })
}

fn boot() -> Daemon {
    let root = ScratchDir::new("ck-health-output");
    for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let module = |id: &str, metrics: Value| {
        json!({
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "env": {
                "FAKE_AFT_MODULE_ID": id,
                "FAKE_AFT_PID_PATH": root.join(format!("{id}.pid")),
                "FAKE_AFT_ADVERTISE_HEALTH": "1",
                "FAKE_AFT_HEALTH_STATUS": "ok",
                "FAKE_AFT_HEALTH_DETAIL": "3 runs in flight",
                "FAKE_AFT_HEALTH_METRICS": metrics.to_string(),
            }
        })
    };
    let config = json!({
        "version": 1,
        "modules": {
            "hl-alpha": module("hl-alpha", alpha_metrics()),
            "hl-beta": module("hl-beta", beta_metrics()),
        }
    });
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let child = dev_command(common::ckdev_subc())
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("CK_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon { root, child };
    let deadline = Instant::now() + Duration::from_secs(20);
    'wait: loop {
        assert!(Instant::now() < deadline, "the modules never registered");
        if daemon.connection().exists() {
            let mut all = true;
            for id in ["hl-alpha", "hl-beta"] {
                let mut command = dev_command(common::ckdev_ck());
                common::kit::isolate_client(&mut command, &daemon.root.join("client"));
                let out = command
                    .arg("--subc")
                    .arg(daemon.connection())
                    .args(["module", "status", id, "--json"])
                    .output()
                    .unwrap();
                all &= serde_json::from_slice::<Value>(&out.stdout)
                    .is_ok_and(|status| status["module"]["live"] == true);
            }
            if all {
                break 'wait;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    daemon
}

const ALPHA_DEFAULT: &str = "hl-alpha: ok\n  3 runs in flight\n  runs_today: 1284\n  rss_bytes: 73.5 KiB\n  catalog_age_ms: 2m\n  lanes: fast=2 · slow=1\n";
const ALPHA_VERBOSE: &str = "hl-alpha: ok\n  3 runs in flight\n  catalog_age_ms: 2m\n  empty: none\n  enabled: enabled\n  headline:\n    - runs_today\n    - rss_bytes\n    - catalog_age_ms\n    - lanes\n  lanes: fast=2 · slow=1\n  roots:\n    - r1\n      count: 3\n    - r2\n      count: 0\n  rss_bytes: 73.5 KiB\n  runs_today: 1284\n  wait_ms: 250 ms\n";
const BETA_DEFAULT: &str = "hl-beta: ok\n  3 runs in flight\n  empty_text: none\n  idle_age_secs: 1m ago\n  label: steady\n  missing: unknown\n  queue: 3\n";
const BETA_VERBOSE: &str = "hl-beta: ok\n  3 runs in flight\n  empty_text: none\n  idle_age_secs: 1m ago\n  label: steady\n  missing: unknown\n  queue: 3\n  sub: a=1 · b=two\n";

#[test]
fn ck_health_prints_the_same_text_after_the_rule_moved() {
    let daemon = boot();
    let cases = [
        (["health", "hl-alpha"].as_slice(), ALPHA_DEFAULT),
        (
            ["health", "hl-alpha", "--verbose"].as_slice(),
            ALPHA_VERBOSE,
        ),
        (["health", "hl-beta"].as_slice(), BETA_DEFAULT),
        (["health", "hl-beta", "--verbose"].as_slice(), BETA_VERBOSE),
    ];
    let mut report = String::new();
    for (args, want) in cases {
        let got = daemon.ck(args);
        if got != want {
            report += &format!("ck {args:?}\n--- got ---\n{got:?}\n--- want ---\n{want:?}\n");
        }
    }
    assert!(report.is_empty(), "{report}");
}
