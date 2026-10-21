//! `ck quota` prints exactly what it printed before the quota module's row
//! parsing moved into `ck_ui::quota` (shared with the app's Usage panel):
//! the default table and `--verbose`, for recorded `usage.get` rows covering
//! every window shape (the three slots and `extraRateWindows`, whose numbers
//! sit under `window`), spend pools, a failing account, and disconnected
//! rows of each class. Accounts are invented; reset times are in the past so
//! the output doesn't move with the clock. The expected text was captured
//! from the code as it was before the move.

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
        if let Some(pid) = fs::read_to_string(self.root.join("insula.pid"))
            .ok()
            .and_then(|p| p.trim().parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
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

fn usage_rows() -> Value {
    json!([
        {
            "provider": "claude",
            "account": "acct-ops",
            "accountInfo": { "email": "ops@example.test", "planType": "max" },
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300, "resetsAt": "2020-01-01T05:00:00Z" },
                "secondary": { "usedPercent": 40.0, "rawUsedPercent": 60.0, "windowMinutes": 10080, "resetsAt": "2020-01-07T00:00:00Z" },
                "extraRateWindows": [
                    { "id": "opus-weekly", "title": "Opus weekly",
                      "window": { "usedPercent": 70.0, "windowMinutes": 10080, "windowKind": "weekly", "resetsAt": "2020-01-07T00:00:00Z" } }
                ]
            }
        },
        {
            "provider": "claude",
            "account": "acct-dev",
            "accountInfo": { "email": "dev@example.test" },
            "error": "token expired",
            "errorClass": "credential_expired"
        },
        {
            "provider": "antigravity",
            "account": "acct-ag",
            "accountInfo": { "email": "ag@example.test" },
            "usage": { "extraRateWindows": [
                { "id": "gemini-weekly", "title": "Gemini Models weekly",
                  "window": { "resetsAt": "2020-01-03T22:00:53Z", "usedPercent": 100.0, "windowKind": "weekly", "windowMinutes": 10080 } },
                { "id": "3p-weekly", "title": "Claude and GPT models weekly",
                  "window": { "resetsAt": "2020-01-01T21:23:30Z", "usedPercent": 100.0, "windowKind": "weekly", "windowMinutes": 10080 } }
            ]}
        },
        {
            "provider": "claude-console",
            "account": "0f1e2d3c-aaaa-bbbb-cccc-000000000001",
            "usage": {},
            "spend": [
                { "basis": "reported", "expiresAt": "2020-01-25T00:00:00Z", "funding": "subscription", "id": "pool-1",
                  "label": "API credit (Max 20x)",
                  "remaining": { "exponent": 2, "minor": 19663, "unit": "USD" },
                  "total": { "exponent": 2, "minor": 20000, "unit": "USD" } },
                { "basis": "reported", "funding": "purchased", "id": "pool-2", "label": "Bought credit",
                  "remaining": { "exponent": 2, "minor": 500, "unit": "USD" } }
            ]
        },
        {
            "provider": "deepseek",
            "usage": {},
            "spend": [{ "label": "Balance", "remaining": { "exponent": 2, "minor": 2402, "unit": "CNY" } }]
        },
        { "provider": "codebuff", "error": "no session", "errorClass": "credential_absent" },
        { "provider": "jetbrains", "error": "no quota reported", "errorClass": "no_quota_reported" },
        { "provider": "kimi", "error": "the quota module crashed", "errorClass": "internal_error" }
    ])
}

fn boot() -> Daemon {
    let root = ScratchDir::new("ck-quota-output");
    for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let config = json!({
        "version": 1,
        "modules": {
            "insula": {
                "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "env": {
                    "FAKE_AFT_MODULE_ID": "insula",
                    "FAKE_AFT_ROLE": "management_surface",
                    "FAKE_AFT_PID_PATH": root.join("insula.pid"),
                    "FAKE_AFT_USAGE_GET_FIXTURE": usage_rows().to_string(),
                }
            }
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
    loop {
        assert!(Instant::now() < deadline, "insula never registered");
        if daemon.connection().exists() {
            let mut command = dev_command(common::ckdev_ck());
            common::kit::isolate_client(&mut command, &daemon.root.join("client"));
            let out = command
                .arg("--subc")
                .arg(daemon.connection())
                .args(["module", "status", "insula", "--json"])
                .output()
                .unwrap();
            if serde_json::from_slice::<Value>(&out.stdout)
                .is_ok_and(|status| status["module"]["live"] == true)
            {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    daemon
}

const QUOTA_DEFAULT: &str = "Antigravity — 1 account\n  ● ag@example.test\n      ● Gemini Models weekly          ████████████████  100% used · resets Jan 03 22:00 UTC\n      ● Claude and GPT models weekly  ████████████████  100% used · resets Jan 01 21:23 UTC\n\nClaude — 2 accounts\n  ● ops@example.test · plan: max\n      ● 5h           ████░░░░░░░░░░░░  25% used · resets Jan 01 05:00 UTC\n      ● week         ██████████░░░░░░  60% used (40% eff · resets banked) · resets Jan 07 00:00 UTC\n      ● Opus weekly  ███████████░░░░░  70% used · resets Jan 07 00:00 UTC\n  ○ dev@example.test [credential_expired] — token expired\n\nKimi — 1 account\n  ○ account [quota-module defect] — the quota module crashed\n\nClaude Console — 1 account\n  ● 0f1e2d3c\n      API credit (Max 20x)  196.63 of 200.00 USD left · expires 2020-01-25 · included in subscription\n      Bought credit  5.00 USD left · bought credit\n\nDeepseek — 1 account\n  ● account\n      Balance  24.02 CNY left\n\n2 providers not configured\n";
const QUOTA_VERBOSE: &str = "Antigravity — 1 account\n  ● ag@example.test\n      ● Gemini Models weekly          ████████████████  100% used · resets Jan 03 22:00 UTC\n      ● Claude and GPT models weekly  ████████████████  100% used · resets Jan 01 21:23 UTC\n\nClaude — 2 accounts\n  ● ops@example.test · plan: max\n      ● 5h           ████░░░░░░░░░░░░  25% used · resets Jan 01 05:00 UTC\n      ● week         ██████████░░░░░░  60% used (40% eff · resets banked) · resets Jan 07 00:00 UTC\n      ● Opus weekly  ███████████░░░░░  70% used · resets Jan 07 00:00 UTC\n  ○ dev@example.test [credential_expired] — token expired\n\nCodebuff — 1 account\n  ○ account — no session\n\nJetbrains — 1 account\n  ○ account — no quota reported\n\nKimi — 1 account\n  ○ account [quota-module defect] — the quota module crashed\n\nClaude Console — 1 account\n  ● 0f1e2d3c\n      API credit (Max 20x)  196.63 of 200.00 USD left · expires 2020-01-25 · included in subscription\n      Bought credit  5.00 USD left · bought credit\n\nDeepseek — 1 account\n  ● account\n      Balance  24.02 CNY left\n";

#[test]
fn ck_quota_prints_the_same_text_after_the_row_parser_moved() {
    let daemon = boot();
    let cases = [
        (["quota"].as_slice(), QUOTA_DEFAULT),
        (["quota", "--verbose"].as_slice(), QUOTA_VERBOSE),
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
