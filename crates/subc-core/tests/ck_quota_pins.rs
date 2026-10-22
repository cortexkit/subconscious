//! Pins of what piped `ck quota` prints today, byte for byte: the default
//! table, `--verbose`, `--redact` and `--json`, with stdout, stderr and the
//! exit code.
//!
//! The quota module is the test stub registered as `insula`, answering
//! `usage.get` with the recorded rows in [`usage_rows`]. They cover every
//! row shape `ck quota` prints differently: the three window slots and
//! `extraRateWindows`, a window one account reports and a degraded sibling
//! lacks, spend pools, saved resets, a stale reading, a reading fetched long
//! ago, a degraded connected account, an account named only by its
//! credential source, a UUID account id, a provider that runs only on
//! another machine, and disconnected rows of each class. Accounts carry org
//! names as well as emails, so the `--redact` pin shows both hidden.
//! Accounts, emails and org names are invented.
//!
//! Each command runs against two fresh private daemons (`common::kit`).
//! Some rows reset, expire or were read at fixed dates, so the countdowns and
//! ages printed for them move with the clock; each is masked by a named rule
//! in [`RULES`]. Both masked runs must equal each other and the pin files in
//! `tests/golden/piped/`. Never edit a pin to make this test pass: a changed
//! pin is a changed `ck quota` output, which scripts that read piped `ck`
//! would see.
//!
//! The `--json` pin is compared with its keys sorted, for the reason
//! `ck_piped_pins.rs` gives: the printed key order depends on whether a
//! serde_json feature is on, which differs between a workspace build and a
//! build of this package alone.
//!
//! Unix only, like the other tests that run the stub as a quota module.

#![cfg(unix)]

use std::{fs, path::PathBuf, process::Output};

use serde_json::{json, Map, Value};

mod common;
use common::kit::Kit;

/// The pinned commands, in the order they run, with each pin's file name.
const COMMANDS: &[(&str, &[&str])] = &[
    ("quota", &["quota"]),
    ("quota-verbose", &["quota", "--verbose"]),
    ("quota-redact", &["quota", "--redact"]),
    ("quota.json", &["--json", "quota"]),
];

/// The rows the stub returns from `usage.get`. Dates in 2020 are in the
/// past, so their windows print an absolute reset time that never moves;
/// dates in 2099 are in the future and print a countdown, which is masked.
fn usage_rows() -> Value {
    json!([
        {
            "provider": "claude",
            "account": "acct-ops",
            "accountInfo": {
                "email": "ops@example.test",
                "orgName": "Ops Example Org",
                "planType": "max"
            },
            "fetchedAt": "2020-01-01T00:00:00Z",
            "savedResets": { "availableCount": 2, "soonestExpiresAt": "2099-01-01T00:00:00Z" },
            "usage": {
                "primary": { "usedPercent": 25.0, "windowMinutes": 300, "resetsAt": "2099-01-01T05:00:00Z" },
                "secondary": { "usedPercent": 40.0, "rawUsedPercent": 60.0, "windowMinutes": 10080, "resetsAt": "2020-01-07T00:00:00Z", "usedCount": 10336, "totalCount": 40000 },
                "extraRateWindows": [
                    { "id": "opus-weekly", "title": "Opus weekly",
                      "window": { "usedPercent": 70.0, "windowMinutes": 10080, "windowKind": "weekly", "resetsAt": "2020-01-07T00:00:00Z" } }
                ]
            }
        },
        {
            "provider": "claude",
            "account": "acct-team",
            "accountInfo": { "email": "team@example.test", "orgName": "Team Example Org" },
            "error": "weekly probe failed",
            "stale": { "class": "upstream_timeout", "since": "2020-01-02T00:00:00Z" },
            "usage": {
                "primary": { "usedPercent": 90.0, "windowMinutes": 300, "resetsAt": "2020-01-01T05:00:00Z" }
            }
        },
        {
            "provider": "claude",
            "account": "acct-dev",
            "accountInfo": { "email": "dev@example.test", "orgName": "Dev Example Org" },
            "error": "token expired",
            "errorClass": "credential_expired"
        },
        {
            "provider": "codex",
            "source": "oauth",
            "usage": {
                "primary": { "usedPercent": 12.5, "windowMinutes": 300, "resetsAt": "2020-01-01T05:00:00Z" },
                "tertiary": { "windowMinutes": 43200 }
            }
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
            "provider": "windsurf",
            "account": "acct-ws",
            "error": "no local provider process",
            "errorClass": "local_source_unavailable"
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

fn boot() -> Kit {
    Kit::boot(
        "ck-quota-pins",
        &[(
            "insula",
            json!({
                "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "env": {
                    "FAKE_AFT_MODULE_ID": "insula",
                    "FAKE_AFT_ROLE": "management_surface",
                    "FAKE_AFT_USAGE_GET_FIXTURE": usage_rows().to_string(),
                }
            }),
        )],
    )
}

/// One command's captured output.
struct Run {
    pin: &'static str,
    args: &'static [&'static str],
    output: Output,
}

/// Waits for the quota module, then runs every pinned command once.
fn take_fresh() -> Vec<Run> {
    let kit = boot();
    kit.wait_json(
        "the quota module live",
        &["--json", "module", "status", "insula"],
        |status| status["module"]["live"] == true,
    );
    COMMANDS
        .iter()
        .map(|&(pin, args)| Run {
            pin,
            args,
            output: kit.ck(args),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Masking rules. Each names one kind of value that moves with the clock and
// replaces it with `<rule name>`.

struct Rule {
    name: &'static str,
    /// The text printed just before the value.
    before: &'static str,
}

/// Every masked value is a duration counted from now, printed as one or two
/// units ("26297d", "4h32m", "45s").
const RULES: &[Rule] = &[
    // A window that resets in the future prints how long until it does.
    Rule {
        name: "reset_countdowns",
        before: "resets in ",
    },
    // Saved resets print how long until the soonest one expires.
    Rule {
        name: "saved_reset_expiries",
        before: "soonest expires in ",
    },
    // A reading taken more than 90 seconds ago prints its age.
    Rule {
        name: "reading_ages",
        before: "fetched ",
    },
    // A stale reading prints how long the module has failed to refresh it.
    Rule {
        name: "refresh_failure_ages",
        before: "refresh failing for ",
    },
];

/// Replaces each duration that directly follows `rule.before`.
fn mask_durations(text: &str, rule: &Rule) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(rule.before) {
        let value_start = start + rule.before.len();
        let value_len = rest[value_start..]
            .find(|c: char| !(c.is_ascii_digit() || matches!(c, 'd' | 'h' | 'm' | 's')))
            .unwrap_or(rest.len() - value_start);
        out.push_str(&rest[..value_start]);
        let value = &rest[value_start..value_start + value_len];
        if value.starts_with(|c: char| c.is_ascii_digit()) {
            out.push_str(&format!("<{}>", rule.name));
        } else {
            out.push_str(value);
        }
        rest = &rest[value_start + value_len..];
    }
    out.push_str(rest);
    out
}

/// The same value with every object's keys in sorted order.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let mut out = Map::new();
            for (key, child) in entries {
                out.insert(key.clone(), sorted(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// A run's masked pin text, and the rules that changed it.
fn masked(run: &Run) -> (String, Vec<&'static str>) {
    let mut fired = Vec::new();
    let mut apply = |text: &str| -> String {
        let mut text = text.to_string();
        for rule in RULES {
            let next = mask_durations(&text, rule);
            if next != text {
                fired.push(rule.name);
            }
            text = next;
        }
        text
    };
    let stdout = apply(&String::from_utf8_lossy(&run.output.stdout));
    let stderr = apply(&String::from_utf8_lossy(&run.output.stderr));
    let stdout = if run.pin.ends_with(".json") && run.output.status.success() {
        let value: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{}: stdout is not JSON: {error}\n{stdout}", run.pin));
        assert_eq!(
            stdout,
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
            "{}: stdout is not the value pretty-printed with a trailing newline",
            run.pin
        );
        format!(
            "{}\n",
            serde_json::to_string_pretty(&sorted(&value)).unwrap()
        )
    } else {
        stdout
    };
    let text = format!(
        "$ ck --subc <connection file> {}\nexit: {:?}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
        run.args.join(" "),
        run.output.status.code(),
    );
    (text, fired)
}

fn pin_path(pin: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/piped")
        .join(format!("{pin}.txt"))
}

#[test]
fn piped_ck_quota_prints_its_pinned_bytes_against_two_fresh_kit_daemons() {
    let first = take_fresh();
    let second = take_fresh();

    let mut report = String::new();
    let mut fired_anywhere = Vec::new();
    for (one, two) in first.iter().zip(&second) {
        let (text_one, fired_one) = masked(one);
        let (text_two, fired_two) = masked(two);
        fired_anywhere.extend(fired_one);
        fired_anywhere.extend(fired_two);
        if text_one != text_two {
            report += &format!(
                "{}: the two kits' masked outputs differ\n--- first ---\n{text_one}--- second ---\n{text_two}--- end ---\n",
                one.pin
            );
        }
        let path = pin_path(one.pin);
        match fs::read_to_string(&path) {
            Ok(want) if want == text_one => {}
            Ok(want) => {
                report += &format!(
                "{} differs from its pin\n--- pin ---\n{want}--- now ---\n{text_one}--- end ---\n",
                path.display()
            )
            }
            Err(error) => {
                report += &format!(
                    "{} unreadable ({error})\n--- now ---\n{text_one}--- end ---\n",
                    path.display()
                )
            }
        }
    }
    // A rule that masks nothing is masking nothing that moves; drop it rather
    // than let it hide a future change.
    for rule in RULES {
        if !fired_anywhere.contains(&rule.name) {
            report += &format!("rule {} masked nothing\n", rule.name);
        }
    }
    assert!(report.is_empty(), "{report}");
}
