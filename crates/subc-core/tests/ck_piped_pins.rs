//! Pins of what piped `ck` prints today, byte for byte, for `health`,
//! `module list`, `module restart`, `module rescan --dry-run`, `activity`
//! and `daemon`, plain and `--json`: stdout, stderr and the exit code.
//!
//! Each command runs against two fresh private daemons (`common::kit`), and
//! the values that differ between the two runs are masked, each by a named
//! rule in [`RULES`]. Both masked runs must equal each other and the pin
//! files in `tests/golden/piped/`. Never edit a pin to make this test pass: a
//! changed pin is a changed `ck` output, which scripts that read piped `ck`
//! would see.
//!
//! JSON outputs are pinned with their keys sorted. `ck --json` prints a
//! `serde_json::Value`, whose key order depends on whether serde_json's
//! `preserve_order` feature is on, and a workspace build turns it on through
//! another crate while a build of this package alone does not. So the test
//! checks that the bytes are exactly the pretty-printed value with a trailing
//! newline, and compares the value itself with sorted keys.
//!
//! Linux only: the running-image evidence and the resource readings `module
//! list` and `module restart` print have a different shape on each platform.

#![cfg(target_os = "linux")]

use std::{fs, path::PathBuf, process::Output};

use serde_json::{json, Map, Value};
use subc_control::ClientControlRequest;

mod common;
use common::{kit::Kit, scripted_daemon};

const MODULE: &str = "pin-alpha";

/// The pinned commands, in the order they run, with each pin's file name.
/// `module restart` changes something, so the kit asserts it is the daemon
/// answering before each run of it.
const COMMANDS: &[(&str, &[&str])] = &[
    ("health", &["health"]),
    ("health.json", &["--json", "health"]),
    ("module-list", &["module", "list"]),
    ("module-list.json", &["--json", "module", "list"]),
    ("module-rescan-dry-run", &["module", "rescan", "--dry-run"]),
    (
        "module-rescan-dry-run.json",
        &["--json", "module", "rescan", "--dry-run"],
    ),
    ("daemon", &["daemon"]),
    ("daemon.json", &["--json", "daemon"]),
    ("module-restart", &["module", "restart", MODULE]),
    (
        "module-restart.json",
        &["--json", "module", "restart", MODULE],
    ),
    ("activity", &["activity"]),
    ("activity.json", &["--json", "activity"]),
];

/// One module with health and metrics. After the daemon ends its
/// connection, its process keeps running until the test lets it go
/// (`Kit::release_exit`). `ck module restart` reads the roster right after
/// the daemon acknowledges the restart, so it always finds the module
/// draining with the old process still running, instead of racing that
/// process's exit and the replacement's start.
fn module_entry() -> Value {
    json!({
        "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
        "health": { "cadence_ms": 500 },
        "env": {
            "FAKE_AFT_MODULE_ID": MODULE,
            "FAKE_AFT_HOLD_EXIT_PATH": true,
            "FAKE_AFT_ADVERTISE_HEALTH": "1",
            "FAKE_AFT_HEALTH_STATUS": "ok",
            "FAKE_AFT_HEALTH_DETAIL": "3 runs in flight",
            "FAKE_AFT_HEALTH_METRICS":
                json!({ "headline": ["runs_today"], "runs_today": 12, "queue": 2 }).to_string(),
        }
    })
}

fn boot() -> Kit {
    Kit::boot("ck-piped-pins", &[(MODULE, module_entry())])
}

/// One command's captured output.
struct Run {
    pin: &'static str,
    args: &'static [&'static str],
    output: Output,
}

/// One kit's runs, with the kit's scratch path, which the
/// `kit_temp_paths` rule masks.
struct Taken {
    kit_root: String,
    runs: Vec<Run>,
}

fn module_status(kit: &Kit) -> Result<Value, String> {
    kit.poll_json(
        "module status",
        &["--json", "module", "status", MODULE],
        |_| true,
    )
}

/// Waits until the module's `generation`th process is live.
fn wait_live(kit: &Kit, generation: u64) -> Result<(), String> {
    kit.poll_json(
        "the module live",
        &["--json", "module", "status", MODULE],
        |status| {
            status["module"]["live"] == true && status["module"]["spawn_generation"] == generation
        },
    )
    .map(drop)
}

/// Fails when the module's process exited without being asked to, which
/// every later pin would show.
fn no_crash(kit: &Kit) -> Result<(), String> {
    let status = module_status(kit)?;
    if status["module"]["lifetime_restarts"] == 0 {
        return Ok(());
    }
    Err("the module exited without being asked to".to_string())
}

/// What the kit knows about the module, for a failure message: its status,
/// its retained stderr, its exits and its captured log lines.
fn diagnostics(kit: &Kit) -> String {
    let mut text = String::new();
    for args in [
        &["--json", "module", "status", MODULE][..],
        &["module", "stderr", MODULE],
        &["module", "terminals", MODULE],
        &["module", "logs", MODULE, "-n", "80"],
    ] {
        let output = kit.ck(args);
        text += &format!(
            "--- ck {} ---\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    text + "--- daemon log ---\n" + &kit.daemon_log()
}

/// Waits for the module and its health, then runs the commands in order,
/// waiting before each one for the state the commands before it leave: a
/// live, healthy new process after a restart, and every ledger entry before
/// `activity`.
fn take(kit: &Kit) -> Result<Taken, String> {
    wait_live(kit, 1)?;
    wait_health(kit)?;
    no_crash(kit)?;
    let mut runs = Vec::new();
    let mut generation = 1;
    for &(pin, args) in COMMANDS {
        let restart = matches!(pin, "module-restart" | "module-restart.json");
        if restart {
            kit.assert_answering();
        }
        // The ledger holds the start and both restarts once the second
        // restart's entry lands.
        if pin == "activity" {
            kit.poll_json("three ledger entries", &["--json", "activity"], |entries| {
                entries.as_array().is_some_and(|entries| entries.len() == 3)
            })?;
        }
        runs.push(Run {
            pin,
            args,
            output: kit.ck(args),
        });
        // The old process is held until ck has printed what it read, then let
        // go; the next one must be live and healthy before the next command.
        if restart {
            kit.release_exit(MODULE);
            generation += 1;
            wait_live(kit, generation)?;
            kit.hold_exit(MODULE);
            wait_health(kit)?;
        }
    }
    no_crash(kit)?;
    Ok(Taken {
        kit_root: kit.root().display().to_string(),
        runs,
    })
}

/// Waits until the daemon has recorded the module's health as `ok`.
fn wait_health(kit: &Kit) -> Result<(), String> {
    kit.poll_json("health recorded", &["--json", "health"], |health| {
        health["modules"][0]["status"] == "ok"
    })
    .map(drop)
}

/// Takes the pinned outputs from a fresh kit, failing with the module's
/// status and stderr if its process exited without being asked to.
fn take_fresh() -> Taken {
    let kit = boot();
    take(&kit).unwrap_or_else(|error| panic!("{error}\n{}", diagnostics(&kit)))
}

// ---------------------------------------------------------------------------
// Masking rules. Each names one kind of value that differs between two fresh
// daemons and replaces it with `<rule name>`.

struct Rule {
    name: &'static str,
    /// Masks plain text.
    text: fn(&str, &str) -> String,
    /// JSON keys whose values this rule masks, at any depth, when the value
    /// has the kind `json_kind` accepts.
    json_keys: &'static [&'static str],
    json_kind: fn(&Value) -> bool,
    /// The pins this rule applies to; empty means every pin.
    pins: &'static [&'static str],
}

fn any(_: &Value) -> bool {
    true
}

fn number(value: &Value) -> bool {
    value.is_number()
}

fn string(value: &Value) -> bool {
    value.is_string()
}

fn no_text(text: &str, _: &str) -> String {
    text.to_string()
}

const RULES: &[Rule] = &[
    // The kit's scratch directory, in the connection-file path `module
    // restart` names.
    Rule {
        name: "kit_temp_paths",
        text: |text, kit_root| text.replace(kit_root, "<kit_temp_paths>"),
        json_keys: &[],
        json_kind: any,
        pins: &[],
    },
    // The daemon library's version, at the start of plain `ck daemon`. It
    // changes with every daemon release, so the pin holds the field's place,
    // never one release's number.
    Rule {
        name: "daemon_versions",
        text: |text, _| mask_between(text, "daemon ", " · pid ", "daemon_versions"),
        json_keys: &[],
        json_kind: any,
        pins: &[],
    },
    // The daemon's process id, in plain `ck daemon`.
    Rule {
        name: "pids",
        text: |text, _| mask_between(text, " · pid ", " · ", "pids"),
        json_keys: &[],
        json_kind: any,
        pins: &[],
    },
    // How long the daemon has run, in plain `ck daemon`.
    Rule {
        name: "uptimes",
        text: |text, _| mask_between(text, " · up ", " · ", "uptimes"),
        json_keys: &[],
        json_kind: any,
        pins: &[],
    },
    // Clients connected to the daemon, which counts `ck`'s own connections
    // while they close.
    Rule {
        name: "client_counts",
        text: |text, _| mask_clients(text),
        json_keys: &["connected_clients"],
        json_kind: number,
        pins: &[],
    },
    // The machine id each daemon mints at its first start.
    Rule {
        name: "machine_ids",
        text: |text, _| mask_between(text, "machine id: ", "\n", "machine_ids"),
        json_keys: &["machine_id"],
        json_kind: string,
        pins: &[],
    },
    // Wall-clock times: probes, exits and ledger entries.
    Rule {
        name: "timestamps",
        text: |text, _| mask_utc_times(text),
        json_keys: &["at_ms", "last_probe_ms", "last_exit_ms"],
        json_kind: number,
        pins: &[],
    },
    // The id each daemon start stamps on its ledger entries.
    Rule {
        name: "incarnation_ids",
        text: no_text,
        json_keys: &["daemon_incarnation"],
        json_kind: string,
        pins: &[],
    },
    // The module process's memory and CPU readings.
    Rule {
        name: "resource_counters",
        text: no_text,
        json_keys: &["memory_bytes", "cpu_user_ms", "cpu_system_ms", "swap_bytes"],
        json_kind: number,
        pins: &[],
    },
    // What was built: the daemon's commit and lock digest, and the digest of
    // the module's running executable.
    Rule {
        name: "build_ids",
        text: no_text,
        json_keys: &["build_git_sha", "build_lock_digest", "digest"],
        json_kind: string,
        pins: &[],
    },
    // `ck` prints the capabilities the daemon advertises, and a daemon that
    // gains one must not change this pin; another test checks that every
    // advertised capability is printed.
    Rule {
        name: "capability_lists",
        text: no_text,
        json_keys: &["capabilities"],
        json_kind: Value::is_array,
        pins: &[],
    },
];

/// Replaces the text between each `before` and the next `after` on the same
/// line.
fn mask_between(text: &str, before: &str, after: &str, name: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(before) {
        let value_start = start + before.len();
        let line_end = rest[value_start..]
            .find('\n')
            .map_or(rest.len(), |end| value_start + end);
        let Some(end) = rest[value_start..line_end]
            .find(after)
            .map(|end| value_start + end)
            .or((after == "\n").then_some(line_end))
        else {
            out.push_str(&rest[..value_start]);
            rest = &rest[value_start..];
            continue;
        };
        out.push_str(&rest[..value_start]);
        out.push_str(&format!("<{name}>"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// `· 2 clients ·` in plain `ck daemon`.
fn mask_clients(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.split(" · ")
                .map(|part| match part.strip_suffix(" clients") {
                    Some(count)
                        if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        "<client_counts> clients".to_string()
                    }
                    _ => part.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `2026-10-21 17:35:38.718` in the `ck activity` table.
fn mask_utc_times(text: &str) -> String {
    const SHAPE: &str = "dddd-dd-dd dd:dd:dd.ddd";
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let matches = bytes.len() - i >= SHAPE.len()
            && SHAPE.bytes().zip(&bytes[i..]).all(|(shape, &byte)| {
                if shape == b'd' {
                    byte.is_ascii_digit()
                } else {
                    byte == shape
                }
            });
        if matches {
            out.push_str("<timestamps>");
            i += SHAPE.len();
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Masks `value` in place by `rule`; returns whether anything changed.
fn mask_json(value: &mut Value, rule: &Rule) -> bool {
    let mut changed = false;
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if rule.json_keys.contains(&key.as_str()) && (rule.json_kind)(child) {
                    let masked = if child.is_array() {
                        json!([format!("<{}>", rule.name)])
                    } else {
                        json!(format!("<{}>", rule.name))
                    };
                    changed |= *child != masked;
                    *child = masked;
                } else {
                    changed |= mask_json(child, rule);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= mask_json(item, rule);
            }
        }
        _ => {}
    }
    changed
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

fn applies(rule: &Rule, pin: &str) -> bool {
    rule.pins.is_empty() || rule.pins.contains(&pin)
}

/// A run's masked pin text, and the rules that changed it.
fn masked(run: &Run, kit_root: &str) -> (String, Vec<&'static str>) {
    let mut fired = Vec::new();
    let mut apply = |text: &str| -> String {
        let mut text = text.to_string();
        for rule in RULES.iter().filter(|rule| applies(rule, run.pin)) {
            let next = (rule.text)(&text, kit_root);
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
        let mut value: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{}: stdout is not JSON: {error}\n{stdout}", run.pin));
        assert_eq!(
            stdout,
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
            "{}: stdout is not the value pretty-printed with a trailing newline",
            run.pin
        );
        for rule in RULES.iter().filter(|rule| applies(rule, run.pin)) {
            if mask_json(&mut value, rule) {
                fired.push(rule.name);
            }
        }
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

/// Compares masked texts with their pins; returns a report of every
/// difference, each with the whole text `ck` printed.
fn compare_with_pins(texts: &[(&str, String)]) -> String {
    let mut report = String::new();
    for (pin, text) in texts {
        let path = pin_path(pin);
        match fs::read_to_string(&path) {
            Ok(want) if want == *text => {}
            Ok(want) => {
                report += &format!(
                    "{} differs from its pin\n--- pin ---\n{want}--- now ---\n{text}--- end ---\n",
                    path.display()
                )
            }
            Err(error) => {
                report += &format!(
                    "{} unreadable ({error})\n--- now ---\n{text}--- end ---\n",
                    path.display()
                )
            }
        }
    }
    report
}

#[test]
fn piped_ck_prints_its_pinned_bytes_against_two_fresh_kit_daemons() {
    let first = take_fresh();
    let second = take_fresh();

    let mut report = String::new();
    let mut fired_anywhere = Vec::new();
    let mut texts = Vec::new();
    for (one, two) in first.runs.iter().zip(&second.runs) {
        let (text_one, fired_one) = masked(one, &first.kit_root);
        let (text_two, fired_two) = masked(two, &second.kit_root);
        fired_anywhere.extend(fired_one);
        fired_anywhere.extend(fired_two);
        if text_one != text_two {
            report += &format!(
                "{}: the two kits' masked outputs differ\n--- first ---\n{text_one}--- second ---\n{text_two}--- end ---\n",
                one.pin
            );
        }
        texts.push((one.pin, text_one));
    }
    // A rule that masks nothing is masking nothing that differs; drop it
    // rather than let it hide a future change.
    for rule in RULES {
        if !fired_anywhere.contains(&rule.name) {
            report += &format!("rule {} masked nothing\n", rule.name);
        }
    }
    report += &compare_with_pins(&texts);
    assert!(report.is_empty(), "{report}");
}

/// The `config_path` a describe reply may carry, under a neutral home.
const CONFIG_PATH: &str = "/Users/alex/.config/cortexkit/subc.jsonc";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_ck_daemon_leaves_out_config_path_and_stays_on_its_pins() {
    let kit = tokio::task::spawn_blocking(|| {
        let kit = boot();
        kit.wait_json(
            "the module live",
            &["--json", "module", "status", MODULE],
            |status| status["module"]["live"] == true,
        );
        kit
    })
    .await
    .unwrap();
    let mut describe =
        scripted_daemon::raw_control(&kit.connection(), &ClientControlRequest::ServerDescribe {})
            .await;
    assert!(
        describe.get("config_path").is_none(),
        "the kit's daemon now reports config_path; this test's describe value must still add it"
    );
    describe["config_path"] = json!(CONFIG_PATH);
    let daemon_ver = subc_transport::read_for_client(kit.connection())
        .unwrap()
        .daemon_ver;
    let scripted = scripted_daemon::ScriptedDescribeDaemon::start(
        "ck-describe-config-path",
        &daemon_ver,
        describe,
    )
    .await;

    // A client reading the raw reply sees the field.
    let seen = scripted_daemon::raw_control(
        &scripted.connection_file_path,
        &ClientControlRequest::ServerDescribe {},
    )
    .await;
    assert_eq!(seen["config_path"], CONFIG_PATH);

    let connection = scripted.connection_file_path.clone();
    let texts = tokio::task::spawn_blocking(move || {
        let mut texts = Vec::new();
        for &(pin, args) in COMMANDS {
            if pin != "daemon" && pin != "daemon.json" {
                continue;
            }
            let output = kit
                .ck_command()
                .arg("--subc")
                .arg(&connection)
                .args(args)
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("config_path")
                    && !String::from_utf8_lossy(&output.stdout).contains(CONFIG_PATH),
                "ck {args:?} printed the config path:\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let run = Run { pin, args, output };
            texts.push((pin, masked(&run, &kit.root().display().to_string()).0));
        }
        texts
    })
    .await
    .unwrap();
    let report = compare_with_pins(&texts);
    assert!(report.is_empty(), "{report}");
}

/// The pid the module's current process wrote at start.
fn module_pid(kit: &Kit) -> u32 {
    fs::read_to_string(kit.root().join(format!("{MODULE}.pid")))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The restart pins rely on the module holding its old process after a
/// restart; without the hold, ck's read right after the acknowledgement
/// usually still wins the race, so the pins alone can't show the hold works.
/// Here the read comes a whole second later, long after an unheld process
/// would have exited and been replaced.
#[test]
fn a_restarted_kit_module_stays_draining_until_the_test_releases_it() {
    let kit = boot();
    wait_live(&kit, 1).unwrap_or_else(|error| panic!("{error}\n{}", diagnostics(&kit)));
    let old_pid = module_pid(&kit);
    kit.assert_answering();
    let restart = kit.ck(&["--json", "module", "restart", MODULE]);
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );

    std::thread::sleep(std::time::Duration::from_secs(1));
    let list = kit.wait_json("module list", &["--json", "module", "list"], |_| true);
    let module = &list["modules"][0];
    assert_eq!(
        (module["state"].as_str(), module["spawn_generation"].as_u64()),
        (Some("draining"), Some(1)),
        "a second after the restart the module should still be draining its first process: {module}"
    );
    assert_eq!(module_pid(&kit), old_pid, "no replacement process started");
    assert!(
        PathBuf::from(format!("/proc/{old_pid}")).exists(),
        "the old process {old_pid} still runs"
    );

    kit.release_exit(MODULE);
    wait_live(&kit, 2).unwrap_or_else(|error| panic!("{error}\n{}", diagnostics(&kit)));
    assert_ne!(module_pid(&kit), old_pid);
}
