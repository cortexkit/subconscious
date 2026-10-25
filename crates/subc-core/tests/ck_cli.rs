use cortexkit_test_support::ScratchDir as TempDir;
use std::{
    collections::BTreeMap,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_control::{
    ClientControlRequest, ClientControlResponse, ModuleProtocol, SupervisorEntry,
    SupervisorHealthStatus,
};
use subc_daemon::{
    read_frame, write_frame, Frame, HealthConfig, ModuleSpec, RestartPolicy, SupervisedModule,
    Supervisor, SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget, PROTOCOL_VERSION};
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client,
    scripted_daemon::{ScriptedControlAction, ScriptedControlStep, ScriptedDaemon},
    start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_named("ck-cli-server").await
    }

    async fn start_named(name: &str) -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            name,
            process_liveness.clone(),
            supervisor_handle.clone(),
        )
        .await;
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_reports_and_renders_route_counters() {
    let server = TestServer::start().await;

    let response = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["daemon", "--json"],
    ));
    assert_eq!(response["counters"]["module_frames_dropped_no_route"], 0);
    assert_eq!(response["counters"]["route_release_stale_skipped"], 0);

    // The `ck daemon --json` above has exited, but its connection counts until
    // the daemon reads the close. The line below prints the count.
    wait_for_connected_clients(&server, 0).await;
    let uptime_before = connection_file_elapsed(&server.connection_file_path);
    let output = ck_with_subc(&server.connection_file_path, ["daemon"]);
    assert_exit(&output, 0);
    let uptime_after = connection_file_elapsed(&server.connection_file_path);
    assert_rendered_with_uptime(&output, &uptime_before, &uptime_after, |uptime| {
        format!(
            // The in-process test daemon is built without a machine id, so the
            // line says the id is not reported rather than omitting it.
            "daemon test-subc · pid {} · up {uptime} · 1 clients · no frame drops in the last 10 minutes\nmachine id: not reported by this daemon\n",
            std::process::id()
        )
    });

    let verbose = ck_with_subc(&server.connection_file_path, ["daemon", "--verbose"]);
    assert_exit(&verbose, 0);
    let stdout = text(&verbose.stdout);
    assert!(stdout.contains("counter"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("module_frames_dropped_no_route"),
        "stdout:\n{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_ck_renders_live_dashboard_and_commands() {
    let server = TestServer::start().await;
    let output = ck_with_subc(&server.connection_file_path, []);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with(&format!(
            "ck {} · daemon running (pid {}, up ",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        )),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("modules: none"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commands:"), "stdout:\n{stdout}");
    assert!(
        stdout.ends_with("run `ck <command>` for its verbs, `ck --help` for everything\n"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn bare_json_prints_the_one_top_help() {
    let empty_path = TempDir::new("ck-bare-json-path");
    let output = ck_command()
        .arg("--json")
        .env("PATH", empty_path.path())
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "ck — CortexKit operator CLI\n\nusage:\n  ck [--subc <connection-file>] [--json] <domain> [<verb>] [<args>]\n\ndomains:\n  setup     plan and apply the managed CortexKit installation\n  upgrade   plan managed component upgrades\n  module    supervised modules: list, status, stderr, terminals, restart, stop, start, rescan, release\n  catalog   what is registered on the wire (not the supervised roster)\n  routes    live consumers for one module or the whole daemon\n  provenance daemon-attested and module-declared build/process facts\n  health    one-line health for every supervised module\n  quota     AI-provider quota and usage windows\n  daemon    daemon version, uptime, connection info, offline triage, and CI lint\n  machine   this machine's id: show it, or adopt a restored one\n  activity  consequential agent and module events\n\nflags:\n  --subc <file>   use a specific connection file (default: auto-discover)\n  --json          raw JSON output instead of tables\n  --verbose       include diagnostic detail and complete metrics\n\nrun 'ck <domain>' with no verb to see that domain's commands\n"
    );
}

/// Runs Cargo's `ck-under-test` through a link named `ckni` in its own temp
/// folder, the way setup places it beside `ck`: a symlink on Unix, a copy
/// named `ckni.exe` on Windows.
fn ckni_command(name: &str) -> (TempDir, Command) {
    let dir = TempDir::new("ckni-link");
    let target = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-under-test"));
    let link = dir.join(name);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(not(unix))]
    fs::copy(&target, &link).unwrap();
    let mut command = Command::new(&link);
    isolate_ck_command(&mut command);
    (dir, command)
}

#[test]
fn ckni_with_no_command_is_a_usage_error_not_the_dashboard() {
    let name = if cfg!(windows) { "CKNI.exe" } else { "ckni" };
    let (_dir, mut command) = ckni_command(name);
    let output = command.output().unwrap();
    assert_exit(&output, 2);
    assert!(output.stdout.is_empty(), "stdout: {}", text(&output.stdout));
    assert_eq!(
        text(&output.stderr),
        "ckni: usage: ckni needs a command; it never opens the terminal app. Run ckni --help for the commands.\nRun ckni --help for the commands.\n"
    );
}

#[test]
fn ckni_through_a_link_runs_the_same_program() {
    let (_dir, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
    let output = command.arg("--version").output().unwrap();
    assert_exit(&output, 0);
    let version = ck_command().arg("--version").output().unwrap();
    assert_eq!(text(&output.stdout), text(&version.stdout));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ckni_decoded_refusal_keeps_its_code_and_ck_keeps_its_stderr() {
    use common::scripted_daemon::{ScriptedControlAction, ScriptedControlStep, ScriptedDaemon};
    for ckni in [false, true] {
        let daemon = ScriptedDaemon::start(
            "ckni-refusal",
            vec![ScriptedControlStep {
                expected: ClientControlRequest::SupervisorHealth {},
                action: ScriptedControlAction::Error(subc_protocol::ErrorBody {
                    code: "fed_not_authorized".into(),
                    message: "access denied".into(),
                    detail: None,
                }),
            }],
        )
        .await;
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        if !ckni {
            command = ck_command();
        }
        let output = command
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .arg("health")
            .output()
            .unwrap();
        assert_exit(&output, 1);
        assert!(output.stdout.is_empty());
        assert_eq!(
            text(&output.stderr),
            if ckni {
                "ckni: fed_not_authorized: access denied\nCheck the refusal above before retrying the command.\n"
            } else {
                "fed_not_authorized — access denied\n"
            }
        );
    }
}

#[test]
fn ckni_parse_errors_are_formatted_even_before_dispatch() {
    let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
    let output = command.arg("--not-a-flag").output().unwrap();
    assert_exit(&output, 2);
    assert!(output.stdout.is_empty());
    assert_eq!(text(&output.stderr), "ckni: usage: unknown flag '--not-a-flag'. Run ck --help.\nRun ckni --help for the commands.\n");
}

#[test]
fn ckni_external_face_passes_ansi_own_json_stderr_and_exit_seven_unchanged() {
    let data = TempDir::new("ckni-external-face");
    let bin = data.join("cortexkit/bin");
    fs::create_dir_all(&bin).unwrap();
    let fixture = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_external-face-fixture"));
    fs::copy(fixture, bin.join(platform_binary("ck-example-face"))).unwrap();
    for ckni in [false, true] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        if !ckni {
            command = ck_command();
        }
        // ck finds external commands in the CortexKit bin folder under the
        // data home: XDG_DATA_HOME on Unix, LOCALAPPDATA on Windows.
        let output = command
            .env("XDG_DATA_HOME", data.path())
            .env("LOCALAPPDATA", data.path())
            .args(["--json", "example-face", "--json"])
            .output()
            .unwrap();
        assert_exit(&output, 7);
        assert_eq!(
            output.stdout,
            b"\x1b[31mface\x1b[0m\n{\"schema\":\"face/v9\",\"ok\":true}\n"
        );
        assert_eq!(output.stderr, b"face error\n");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn ckni_quota_on_a_terminal_matches_piped_ck_without_color() {
    let kit = common::kit::Kit::boot(
        "ckni-quota-pty",
        &[(
            "insula",
            json!({
                "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "env": {
                    "FAKE_AFT_MODULE_ID": "insula", "FAKE_AFT_ROLE": "management_surface",
                    "FAKE_AFT_USAGE_GET_FIXTURE": json!([{
                        "provider":"claude", "account":"acct-ops",
                        "usage":{"primary":{"usedPercent":25.0,"windowMinutes":300,"resetsAt":"2020-01-01T05:00:00Z"}}
                    }]).to_string(),
                }
            }),
        )],
    );
    kit.wait_json(
        "quota module live",
        &["--json", "module", "status", "insula"],
        |status| status["module"]["live"] == true,
    );
    let piped = kit.ck(&["quota"]);
    assert_exit(&piped, 0);
    assert!(piped.stderr.is_empty());
    assert!(
        text(&piped.stdout).contains("25%"),
        "{}",
        text(&piped.stdout)
    );
    let (_link, mut command) = ckni_command("ckni");
    common::kit::isolate_client_fully(&mut command, &kit.client_root());
    command
        .env_remove("NO_COLOR")
        .arg("--subc")
        .arg(kit.connection())
        .arg("quota");
    let terminal = upgrade_terminal_output(&command);
    assert_exit(&terminal, 0);
    assert_eq!(terminal.stdout, piped.stdout);
    assert_eq!(terminal.stderr, piped.stderr);
}

#[test]
fn ckni_three_tables_are_selected_through_the_link_and_other_text_is_unchanged() {
    let kit = common::kit::Kit::boot("ckni-tables", &[]);
    for (args, header) in [
        (vec!["health"], "MODULE  HEALTH  SUMMARY"),
        (
            vec!["module", "list"],
            "MODULE  STATUS  HEALTH  PID  UP (s)  RESTARTS  BUILD  PENDING",
        ),
        (vec!["activity"], "TIME (UTC)  SUBJECT  EVENT  BY"),
    ] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        common::kit::isolate_client(&mut command, &kit.client_root());
        let output = command
            .arg("--subc")
            .arg(kit.connection())
            .args(args)
            .output()
            .unwrap();
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
        assert_eq!(text(&output.stdout).trim_end(), header);
    }
    for args in [
        vec!["module", "logs"],
        vec!["routes"],
        vec!["catalog"],
        vec!["module", "rescan", "--dry-run"],
    ] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        common::kit::isolate_client(&mut command, &kit.client_root());
        let output = command
            .arg("--subc")
            .arg(kit.connection())
            .args(&args)
            .output()
            .unwrap();
        let ck = kit.ck(&args);
        assert_exit(&output, 0);
        assert_eq!(output.stdout, ck.stdout, "{}", args.join(" "));
        assert_eq!(output.stderr, ck.stderr);
    }
}

#[test]
fn ckni_json_wins_over_each_plain_table_and_versions_the_piped_ck_payload() {
    let kit = common::kit::Kit::boot("ckni-json-tables", &[]);
    for (args, schema) in [
        (&["health"][..], "ck.health/v1"),
        (&["module", "list"][..], "ck.module.list/v1"),
        (&["activity"][..], "ck.activity/v1"),
    ] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        common::kit::isolate_client_fully(&mut command, &kit.client_root());
        let output = command
            .arg("--subc")
            .arg(kit.connection())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        let mut ck_args = vec!["--json"];
        ck_args.extend_from_slice(args);
        let ck = kit.ck(&ck_args);
        assert_exit(&ck, 0);
        assert_exit(&output, 0);
        assert!(ck.stderr.is_empty(), "{}", text(&ck.stderr));
        assert_eq!(output.stderr, ck.stderr, "--json {}", args.join(" "));
        let mut versioned: Value =
            serde_json::from_slice(&output.stdout).expect("ckni --json prints JSON");
        assert_eq!(
            versioned.as_object_mut().unwrap().remove("schema"),
            Some(json!(schema))
        );
        let legacy: Value = serde_json::from_slice(&ck.stdout).unwrap();
        if args == ["activity"] {
            assert_eq!(versioned, json!({"entries": legacy}));
        } else {
            assert_eq!(versioned, legacy);
        }
    }
}

#[test]
fn ckni_json_rejects_prose_help_and_version_before_discovery() {
    for (args, name) in [
        (vec!["setup", "--dry-run"], "setup"),
        (vec!["upgrade", "--check"], "upgrade"),
        (vec!["daemon", "lint"], "daemon lint"),
        (vec!["--help"], "help"),
        (vec!["module", "--help"], "help"),
        (vec!["--version"], "version"),
        (vec![], "help"),
    ] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        let output = command.arg("--json").args(&args).output().unwrap();
        assert_exit(&output, 2);
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_eq!(
            text(&output.stderr),
            format!("ckni: usage: {name} has no JSON form\nRun ckni --help for the commands.\n")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ckni_json_exemptions_keep_probes_and_action_acks_byte_identical() {
    for flag in ["--ck-build-shape", "--ck-cli-face-reserved-names"] {
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        let ckni = command.args(["--json", flag]).output().unwrap();
        let ck = ck_command().args(["--json", flag]).output().unwrap();
        assert_exit(&ckni, 0);
        assert_exit(&ck, 0);
        assert_eq!(ckni.stdout, ck.stdout);
        assert_eq!(ckni.stderr, ck.stderr);
    }
    for args in [
        vec!["restart", "alpha"],
        vec!["restart", "alpha", "--swap"],
        vec!["stop", "alpha"],
        vec!["start", "alpha"],
    ] {
        let steps = || {
            let reply = |expected, response| ScriptedControlStep {
                expected,
                action: ScriptedControlAction::Reply(Box::new(response)),
            };
            let list = || {
                reply(
                    ClientControlRequest::SupervisorList {},
                    ClientControlResponse::SupervisorList {
                        generation: 1,
                        modules: vec![SupervisorEntry::new(
                            "alpha",
                            "starting",
                            true,
                            false,
                            SupervisorHealthStatus::Unknown,
                        )],
                    },
                )
            };
            let request = match args.as_slice() {
                ["restart", "alpha"] => ClientControlRequest::SupervisorRestart {
                    module_id: "alpha".into(),
                    drain_timeout_ms: None,
                },
                ["restart", "alpha", "--swap"] => ClientControlRequest::SupervisorSwap {
                    module_id: "alpha".into(),
                    ready_timeout_ms: None,
                },
                _ => ClientControlRequest::SupervisorSetEnabled {
                    module_id: "alpha".into(),
                    enabled: args[0] == "start",
                },
            };
            let mut steps = vec![];
            if args[0] != "restart" {
                steps.push(list());
            }
            steps.push(reply(
                request,
                ClientControlResponse::SupervisorAck {
                    module_id: "alpha".into(),
                    applied: true,
                },
            ));
            steps.push(list());
            steps
        };
        let daemon = ScriptedDaemon::start_sessions("ckni-ack-json", vec![steps(), steps()]).await;
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        let ckni = command
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .args(["--json", "module"])
            .args(&args)
            .output()
            .unwrap();
        let ck = ck_command()
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .args(["--json", "module"])
            .args(&args)
            .output()
            .unwrap();
        assert_exit(&ckni, 0);
        assert_exit(&ck, 0);
        assert_eq!(ckni.stdout, ck.stdout, "{args:?}");
        assert_eq!(ckni.stderr, ck.stderr, "{args:?}");
        let body: Value = serde_json::from_slice(&ckni.stdout).unwrap();
        assert_eq!(body["applied"], true);
        assert!(body.get("schema").is_none());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ck_json_terminal_health_list_and_restart_are_exactly_piped_without_progress() {
    for args in [
        vec!["health"],
        vec!["module", "list"],
        vec!["module", "restart", "alpha"],
    ] {
        let steps = || {
            let reply = |expected, response| ScriptedControlStep {
                expected,
                action: ScriptedControlAction::Reply(Box::new(response)),
            };
            let list = || ClientControlResponse::SupervisorList {
                generation: 1,
                modules: vec![SupervisorEntry::new(
                    "alpha",
                    "starting",
                    true,
                    false,
                    SupervisorHealthStatus::Unknown,
                )],
            };
            match args.as_slice() {
                ["health"] => vec![reply(
                    ClientControlRequest::SupervisorHealth {},
                    ClientControlResponse::SupervisorHealth {
                        generation: 1,
                        modules: vec![],
                    },
                )],
                ["module", "list"] => vec![reply(ClientControlRequest::SupervisorList {}, list())],
                _ => vec![
                    reply(
                        ClientControlRequest::SupervisorRestart {
                            module_id: "alpha".into(),
                            drain_timeout_ms: None,
                        },
                        ClientControlResponse::SupervisorAck {
                            module_id: "alpha".into(),
                            applied: true,
                        },
                    ),
                    reply(ClientControlRequest::SupervisorList {}, list()),
                ],
            }
        };
        let daemon = ScriptedDaemon::start_sessions("ck-json-pty", vec![steps(), steps()]).await;
        let mut command = ck_command();
        command
            .env_remove("NO_COLOR")
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .arg("--json")
            .args(&args);
        let piped = command.output().unwrap();
        let terminal = upgrade_terminal_output(&command);
        assert_exit(&piped, 0);
        assert_exit(&terminal, 0);
        assert!(piped.stderr.is_empty());
        assert_eq!(terminal.stdout, piped.stdout, "{args:?}");
        assert_eq!(terminal.stderr, piped.stderr, "{args:?}");
        assert!(!terminal.stdout.contains(&0x1b));
        let body: Value = serde_json::from_slice(&terminal.stdout).unwrap();
        assert!(
            body.get("schema").is_none(),
            "ck keeps its existing JSON shape"
        );
        if args.contains(&"restart") {
            assert_eq!(body["applied"], true);
            assert_eq!(
                body["state"], "starting",
                "returns at the ack, not at healthy"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ckni_json_log_follow_versions_each_initial_and_appended_line() {
    use std::io::Write;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let data = TempDir::new("ckni-log-json");
    let logs = data.join("cortexkit/alpha/logs");
    fs::create_dir_all(&logs).unwrap();
    let path = logs.join("alpha.2026-01-01.log");
    fs::write(&path, "2026-01-01T00:00:00.000Z INFO  alpha: initial\n").unwrap();
    let daemon = ScriptedDaemon::start(
        "ckni-log-json",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::Reply(Box::new(ClientControlResponse::SupervisorList {
                generation: 1,
                modules: vec![scripted_supervisor_entry("alpha", None)],
            })),
        }],
    )
    .await;
    let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
    command
        .env("XDG_DATA_HOME", data.path())
        .arg("--subc")
        .arg(&daemon.connection_file_path)
        .args(["--json", "module", "logs", "alpha", "-f"]);
    let mut command = tokio::process::Command::from(command);
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let initial = timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial: Value = serde_json::from_str(&initial).expect("one complete object per line");
    assert_eq!(initial["schema"], "ck.module.logs/v1");
    assert!(initial["line"].as_str().unwrap().ends_with("initial"));
    assert!(initial.get("lines").is_none());
    // The follower snapshots the file end after the initial output. Let that
    // snapshot complete before appending the line whose delivery is asserted.
    tokio::time::sleep(Duration::from_millis(500)).await;
    writeln!(
        fs::OpenOptions::new().append(true).open(&path).unwrap(),
        "2026-01-01T00:00:01.000Z INFO  alpha: appended"
    )
    .unwrap();
    let appended = timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let appended: Value = serde_json::from_str(&appended).unwrap();
    assert_eq!(appended["schema"], "ck.module.logs/v1");
    assert!(appended["line"].as_str().unwrap().ends_with("appended"));
    child.kill().await.unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert_eq!(text(&output.stderr), "showing 1 of 1 lines\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ckni_json_empty_logs_are_arrays_and_unknown_modules_are_refused() {
    for known in [true, false] {
        let data = TempDir::new("ckni-empty-log-json");
        let daemon = ScriptedDaemon::start(
            "ckni-empty-log-json",
            vec![ScriptedControlStep {
                expected: ClientControlRequest::SupervisorList {},
                action: ScriptedControlAction::Reply(Box::new(
                    ClientControlResponse::SupervisorList {
                        generation: 1,
                        modules: if known {
                            vec![scripted_supervisor_entry("alpha", None)]
                        } else {
                            vec![]
                        },
                    },
                )),
            }],
        )
        .await;
        let (_link, mut command) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        let output = command
            .env("XDG_DATA_HOME", data.path())
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .args(["--json", "module", "logs", "alpha"])
            .output()
            .unwrap();
        if known {
            assert_exit(&output, 0);
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                json!({"schema":"ck.module.logs/v1","lines":[]})
            );
            assert_eq!(text(&output.stderr), "showing 0 of 0 lines\n");
        } else {
            assert_exit(&output, 1);
            assert!(output.stdout.is_empty());
            assert!(text(&output.stderr)
                .starts_with("ckni: failed: module_id 'alpha' is not supervised\n"));
        }
    }
}

#[test]
fn hidden_cli_face_reserved_names_list_every_builtin_domain() {
    let output = ck_command()
        .arg("--ck-cli-face-reserved-names")
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let output_text = text(&output.stdout);
    let names = output_text.lines().collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "activity",
            "catalog",
            "daemon",
            "health",
            "help",
            "machine",
            "module",
            "provenance",
            "quota",
            "routes",
            "setup",
            "upgrade",
        ]
    );
    assert!(output.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_ck_renders_degraded_module_and_no_updates_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_DETAIL", "1 provider failing"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;
    // Only the module's own connection may remain: the health wait's last
    // connection counts until the daemon reads its close, and the summary
    // below prints the count.
    wait_for_connected_clients(&server, 1).await;

    let cache_dir = TempDir::new("ck-dashboard-current-cache");
    let cache = cache_dir.path().join("update-metadata.json");
    fs::write(
        &cache,
        format!(
            r#"{{"format_version":3,"checked_at_unix_secs":{},"targets":{{}}}}"#,
            now_ms() / 1_000
        ),
    )
    .unwrap();
    let uptime_before = connection_file_elapsed(&server.connection_file_path);
    let output = ck_command()
        .args(["--subc"])
        .arg(&server.connection_file_path)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let uptime_after = connection_file_elapsed(&server.connection_file_path);
    assert_rendered_with_uptime(&output, &uptime_before, &uptime_after, |uptime| {
        format!(
            "ck {} · daemon running (pid {}, up {uptime}, 2 clients)\nmodules: insula degraded (1 provider failing)\nupdates: none\n\ncommands: setup · upgrade · module · health · routes · quota · daemon\nrun `ck <command>` for its verbs, `ck --help` for everything\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        )
    });

    module.stop().await.unwrap();
}

#[test]
fn bare_ck_degrades_to_domains_when_daemon_is_unreachable() {
    let missing = unique_temp_dir("ck-bare-missing").join("subc-connection.json");
    let output = ck_command()
        .args(["--subc"])
        .arg(&missing)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with(&format!(
            "ck {} · daemon stopped",
            env!("CARGO_PKG_VERSION")
        )),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&missing.display().to_string()),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commands:"), "stdout:\n{stdout}");
    assert!(stdout.contains("module"), "stdout:\n{stdout}");
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
}

#[test]
fn bare_ck_hanging_release_source_uses_stale_cache_within_the_refresh_budget() {
    let temp = TempDir::new("ck-update-timeout");
    let cache_path = temp.path().join("update-metadata.json");
    fs::write(
        &cache_path,
        r#"{"format_version":3,"checked_at_unix_secs":0,"targets":{}}"#,
    )
    .unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let source_base = format!("http://{}", listener.local_addr().unwrap());
    let (release_done_tx, release_done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = std::io::Read::read(&mut stream, &mut request);
        let _ = release_done_rx.recv_timeout(Duration::from_secs(90));
    });

    let missing = temp.path().join("subc-connection.json");
    let started = std::time::Instant::now();
    let output = ck_command()
        .args(["--subc"])
        .arg(&missing)
        .env("CK_UPDATE_CACHE_PATH", &cache_path)
        .env("CK_RELEASE_INDEX_URL", format!("{source_base}/index.json"))
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    release_done_tx.send(()).unwrap();
    server.join().unwrap();

    assert_exit(&output, 0);
    // Discriminating margin, not a latency SLO: a ck that BLOCKS on the hanging
    // source cannot return before the server's 90s hold releases, while an
    // unblocked ck is back in about a second on unix. On Windows the refresh
    // budget starts only after powershell.exe has started, which has been
    // measured at 5s on a VM and 22s on a loaded hosted runner, so the bound
    // sits far above that and far below the hold.
    assert!(
        elapsed < Duration::from_secs(45),
        "bare ck exceeded its bounded refresh envelope: {elapsed:?}"
    );
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
}

fn serve_index(body: &[u8], signature_header: Option<&str>) -> String {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_vec();
    let signature_header = signature_header.map(str::to_string);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut request = [0_u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let mut header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            if let Some(signature) = &signature_header {
                header.push_str(&format!("X-CortexKit-Signature-Ed25519: {signature}\r\n"));
            }
            header.push_str("\r\n");
            let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
            let _ = std::io::Write::write_all(&mut stream, &body);
        }
    });
    format!("http://{addr}/index.json")
}

fn fixture_index_body() -> Vec<u8> {
    format!(
        r#"{{"schema":1,"channel":"alpha","generated_at_ms":{},"components":{{"core":{{"release":"subc-core-v{}","version":"{}","assets":{{}}}}}}}}"#,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_VERSION"),
    )
    .into_bytes()
}

/// RFC 8032 test-key signature over `{{"schema":1}}` — valid Ed25519, wrong key.
const FOREIGN_SIGNATURE: &str =
    "fjYZ87Tka7M+yJ+lmjD7vjSjflypCGi2KIvmSktgssO79FN8/mntGhobmTCwYDeQRAEAu7oDdv7zrAkI9N9uDA==";

/// A bare `ck setup --dry-run` for the index-refusal tests. The host's own
/// service manager answers for the real user regardless of HOME (launchd
/// knows the developer's daemon), so the runtime is faked the same way the
/// fixtures do; otherwise the refusal under test is masked by a registry
/// read against whatever daemon the developer happens to be running.
fn bare_setup_dry_run(index_url: &str) -> Command {
    let mut command = ck_command();
    command
        .args(["setup", "--dry-run"])
        .env("CK_RELEASE_INDEX_URL", index_url)
        .env("CK_TEST_SETUP_CONTROL_OK", "1")
        .env("CK_TEST_SETUP_MODULES", "")
        .env("SUBC_CONNECTION_FILE", "/nonexistent/subc-connection.json");
    command
}

#[test]
fn setup_dry_run_refuses_when_the_signature_header_is_stripped() {
    let url = serve_index(&fixture_index_body(), None);
    let output = bare_setup_dry_run(&url).output().unwrap();
    assert_ne!(output.status.code(), Some(0), "stripped header must refuse");
    let combined = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        combined.contains("index_signature_invalid"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        combined.contains("generation 1"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        !combined.contains("setup plan:"),
        "stripped header must plan nothing:\n{combined}"
    );
}

#[test]
fn production_ck_ignores_the_test_release_index_key() {
    // The shipped `ck` target, run as `ckdev-ck` like every test process.
    let production = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck"));
    let shape = cortexkit_test_support::dev_command(&production)
        .arg("--ck-build-shape")
        .output()
        .expect("production ck build shape");
    let shape = String::from_utf8_lossy(&shape.stdout);
    if shape.trim() == "test-support: on" {
        eprintln!(
            "skipping production-key control: {} was built with test-support for this invocation",
            production.display()
        );
        return;
    }
    assert_eq!(
        shape.trim(),
        "test-support: off",
        "production ck returned an unknown build shape: {shape:?}"
    );

    let index = serve_signed_index(|base| setup_index(base, None));
    let temp = TempDir::new("production-ck-release-key");
    let home = temp.path().join("home");
    let data_home = home.join(".local").join("share");
    let config_home = home.join(".config");
    let tools = temp.path().join("tools");
    fs::create_dir_all(&tools).expect("service-manager fixture directory");
    // The service manager must answer "registered but not running" so setup
    // skips the live-module probe and reaches the release-index check. On
    // macOS that is a successful `launchctl print` whose output lacks
    // `state = running`; on Linux liveness is `systemctl --user is-active`
    // SUCCEEDING, so the fixture has to fail that verb (3 = inactive) while
    // still succeeding for `is-enabled`. A plain `exit 0` reads as a live
    // daemon on Linux and sends setup to `ck module list` against no daemon.
    write_executable(
        &tools.join(service_manager_program()),
        "#!/bin/sh\ncase \"$*\" in *is-active*) exit 3;; esac\nexit 0\n",
    );
    let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .expect("fixture PATH");
    let output = cortexkit_test_support::dev_command(&production)
        .args(["setup", "--dry-run"])
        .env("CK_RELEASE_INDEX_URL", &index.url)
        .env("CK_TEST_RELEASE_INDEX_PUBKEY", &index.public_key)
        .env(
            "CK_UPDATE_CACHE_PATH",
            temp.path().join("update-metadata.json"),
        )
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", &data_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("PATH", path)
        .env("SUBC_CONNECTION_FILE", home.join("no-daemon.json"))
        .output()
        .expect("production ck launches");

    assert_ne!(output.status.code(), Some(0), "foreign key must refuse");
    let combined = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        combined.contains("index_signature_invalid"),
        "stdout+stderr:\n{combined}"
    );
}

#[test]
fn setup_dry_run_refuses_a_test_key_signature_against_the_embedded_key() {
    let url = serve_index(&fixture_index_body(), Some(FOREIGN_SIGNATURE));
    let output = bare_setup_dry_run(&url).output().unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "foreign signature must refuse"
    );
    let combined = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        combined.contains("index_signature_invalid"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        combined.contains("generation 1"),
        "stdout+stderr:\n{combined}"
    );
}

struct SetupFixture {
    _root: TempDir,
    home: PathBuf,
    data_home: PathBuf,
    config_home: PathBuf,
    tools: PathBuf,
}

impl SetupFixture {
    fn installed(name: &str) -> Self {
        Self::new(name, true)
    }

    // Used only by the setup-apply tests, which are gated off Windows above.
    #[cfg(not(windows))]
    fn fresh(name: &str) -> Self {
        Self::new(name, false)
    }

    fn new(name: &str, installed: bool) -> Self {
        let root = TempDir::new(name);
        let home = root.path().join("home");
        let data_home = home.join(".local").join("share");
        let config_home = home.join(".config");
        let cortexkit = data_home.join("cortexkit");
        let bin = cortexkit.join("bin");
        let config = config_home.join("cortexkit").join("subc.jsonc");
        let tools = root.path().join("tools");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&tools).unwrap();
        write_executable(
            &tools.join(service_manager_program()),
            service_manager_script(),
        );

        if installed {
            let binaries = [
                ("core", "ck-subc"),
                ("core", "ck-subc-mcp"),
                ("aft", "ck-aft"),
                ("claustrum", "ck-claustrum"),
                ("claustrum", "ck-auth"),
                ("insula", "ck-insula"),
            ];
            let mut mutations = binaries
                .iter()
                .map(|(component, binary)| {
                    let path = bin.join(platform_binary(binary));
                    fs::write(&path, binary).unwrap();
                    json!({
                        "kind": "managed-binary",
                        "path": path,
                        "component": component,
                    })
                })
                .collect::<Vec<_>>();
            mutations.push(json!({
                "kind": "runtime-definition",
                "path": runtime_definition(&home),
            }));
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(
                &config,
                serde_json::to_string_pretty(&json!({
                    "version": 1,
                    "storage": {"backend": "sqlite"},
                    "modules": {
                        "aft": {"program": bin.join(platform_binary("ck-aft"))},
                        "claustrum": installed_claustrum_entry(&bin, &data_home),
                        "insula": {"program": bin.join(platform_binary("ck-insula"))},
                    },
                }))
                .unwrap(),
            )
            .unwrap();
            fs::create_dir_all(&cortexkit).unwrap();
            fs::write(
                cortexkit.join("installer-manifest.json"),
                serde_json::to_vec_pretty(&json!({
                    "schema_version": 1,
                    "platform": host_target(),
                    "mutations": mutations,
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let fixture = Self {
            _root: root,
            home,
            data_home,
            config_home,
            tools,
        };
        if installed {
            fixture.write_runtime_definition(false);
        }
        fixture
    }

    fn write_runtime_definition(&self, outdated: bool) {
        let path = runtime_definition(&self.home);
        // Each component needs its own join: a slash embedded in a component
        // survives Windows stringification, unlike the inventory's separators.
        let daemon = self
            .data_home
            .join("cortexkit")
            .join("bin")
            .join(platform_binary("ck-subc"));
        let raw = daemon.to_string_lossy();
        let xml = raw
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;");
        let definition = if cfg!(target_os = "macos") {
            let recovery = if outdated {
                ""
            } else {
                "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>10</integer>"
            };
            format!("<plist version=\"1.0\"><dict><key>Label</key><string>cortexkit.subc</string><key>ProgramArguments</key><array><string>{xml}</string></array><key>RunAtLoad</key><true/>{recovery}<key>ExitTimeOut</key><integer>35</integer></dict></plist>")
        } else if cfg!(windows) {
            let user = format!(
                "{}\\{}",
                std::env::var("USERDOMAIN").unwrap_or_default(),
                std::env::var("USERNAME").unwrap_or_default()
            )
            .replace('&', "&amp;");
            let recovery = if outdated {
                ""
            } else {
                "<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>255</Count></RestartOnFailure></Settings>"
            };
            format!("<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><RegistrationInfo><URI>\\CortexKit\\subc-daemon</URI></RegistrationInfo><Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers><Principals><Principal id=\"Author\"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>{recovery}<Actions Context=\"Author\"><Exec><Command>{xml}</Command></Exec></Actions></Task>")
        } else {
            let daemon = raw
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$");
            let timeout = if outdated { "" } else { "TimeoutStopSec=35\n" };
            format!("[Unit]\nDescription=CortexKit subconscious daemon\n[Service]\nExecStart=\"{daemon}\"\nRestart=on-failure\nDelegate=yes\nDelegateSubgroup=daemon\nKillMode=mixed\n{timeout}[Install]\nWantedBy=default.target\n")
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, definition).unwrap();
    }

    fn command(&self, index: &SignedIndex, args: &[&str]) -> Command {
        #[cfg(unix)]
        let command = direct_ck_command();
        #[cfg(not(unix))]
        let command = ck_command();
        self.command_from(command, index, args)
    }

    #[cfg(unix)]
    fn command_direct(&self, index: &SignedIndex, args: &[&str]) -> Command {
        self.command_from(direct_ck_command(), index, args)
    }

    fn command_from(&self, mut command: Command, index: &SignedIndex, args: &[&str]) -> Command {
        let path = std::env::join_paths(std::iter::once(self.tools.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        command
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", &self.data_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", self._root.path().join("runtime"))
            .env("PATH", path)
            // Daemon discovery falls back to the system temp directory, which
            // is outside this fixture's HOME: without the fence a setup path
            // that reaches for a daemon finds the developer's live one and
            // passes here while failing on a runner with no daemon.
            .env("SUBC_CONNECTION_FILE", self.home.join("no-daemon.json"))
            .env("CK_RELEASE_INDEX_URL", &index.url)
            .env("CK_TEST_RELEASE_INDEX_PUBKEY", &index.public_key)
            .env("CK_TEST_SETUP_CONTROL_OK", "1")
            .env("CK_TEST_SETUP_MODULES", "aft,claustrum,insula");
        if !runtime_definition(&self.home).exists() {
            command.env("CK_TEST_SETUP_RUNTIME_MISSING", "1");
        }
        command
    }

    #[cfg(unix)]
    fn command_with_zero_umask(&self, index: &SignedIndex, args: &[&str]) -> Command {
        let child = self.command_direct(index, args);
        let mut command = Command::new(env!("CARGO_BIN_EXE_mode-umask-helper"));
        command.arg(child.get_program()).args(child.get_args());
        for (key, value) in child.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }
}

/// The claustrum entry `ck setup claustrum` writes on this host: macOS keeps
/// the master key in the keychain, every other platform records its file path
/// in the module environment. A fixture that omits the path reads as
/// misconfigured (and so "not installed") everywhere except macOS.
fn installed_claustrum_entry(bin: &Path, data_home: &Path) -> Value {
    let mut entry = json!({
        "program": bin.join(platform_binary("ck-claustrum")),
        "reserved": true,
    });
    if !cfg!(target_os = "macos") {
        entry["env"] = json!({
            "CK_MASTER_KEY_PATH": data_home
                .join("cortexkit")
                .join("claustrum")
                .join("master.key"),
        });
    }
    entry
}

struct SignedIndex {
    url: String,
    public_key: String,
    // Only the Unix-only setup refusal tests assert which URLs were fetched.
    #[cfg_attr(not(unix), allow(dead_code))]
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

fn serve_signed_index(
    build: impl FnOnce(&str) -> (Value, BTreeMap<String, Vec<u8>>),
) -> SignedIndex {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (document, assets) = build(&base);
    let body = serde_json::to_vec(&document).unwrap();
    let signing = SigningKey::from_bytes(&[0x42; 32]);
    let signature =
        base64::engine::general_purpose::STANDARD.encode(signing.sign(&body).to_bytes());
    let public_key = base64::engine::general_purpose::STANDARD.encode(signing.verifying_key());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let served_requests = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut request = [0_u8; 4096];
            let read = std::io::Read::read(&mut stream, &mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            served_requests.lock().unwrap().push(path.to_string());
            let (response, signature_header) = if path == "/index.json" {
                (&body, Some(signature.as_str()))
            } else if let Some(asset) = assets.get(path) {
                (asset, None)
            } else {
                let response = b"not found".to_vec();
                let header = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.len()
                );
                let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
                let _ = std::io::Write::write_all(&mut stream, &response);
                continue;
            };
            let mut header = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n",
                response.len()
            );
            if let Some(signature) = signature_header {
                header.push_str(&format!("X-CortexKit-Signature-Ed25519: {signature}\r\n"));
            }
            header.push_str("\r\n");
            let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
            let _ = std::io::Write::write_all(&mut stream, response);
        }
    });
    SignedIndex {
        url: format!("{base}/index.json"),
        public_key,
        requests,
    }
}

fn setup_index(base: &str, mc_archive: Option<&[u8]>) -> (Value, BTreeMap<String, Vec<u8>>) {
    let target = host_target();
    let dummy = fixture_asset(format!("{base}/unused.zip"), "00".repeat(32), 1, None);
    let mut components = serde_json::Map::new();
    components.insert(
        "core".to_string(),
        index_component(
            "subc-core-v0.17.9",
            Some("0.17.9"),
            &target,
            [("ck-subc", dummy.clone()), ("ck-subc-mcp", dummy.clone())],
        ),
    );
    components.insert(
        "synapse".to_string(),
        json!({"release": "v0.1.0", "version": "0.1.0", "assets": {}}),
    );
    let mut assets = BTreeMap::new();
    if let Some(archive) = mc_archive {
        let path = "/ck-mc.zip";
        components.insert(
            "mc".to_string(),
            index_component(
                "ck-mc-alpha.22464bf2",
                None,
                &target,
                [(
                    "ck-mc",
                    fixture_asset(
                        format!("{base}{path}"),
                        sha256_hex(archive),
                        archive.len() as u64,
                        Some("ck-mc-alpha.22464bf2"),
                    ),
                )],
            ),
        );
        assets.insert(path.to_string(), archive.to_vec());
    }
    (
        json!({
            "schema": 1,
            "channel": "alpha",
            "generated_at_ms": now_ms(),
            "components": components,
        }),
        assets,
    )
}

fn index_component<const N: usize>(
    release: &str,
    version: Option<&str>,
    target: &str,
    assets: [(&str, Value); N],
) -> Value {
    let binaries = assets
        .into_iter()
        .map(|(name, asset)| (name.to_string(), asset))
        .collect::<serde_json::Map<_, _>>();
    let mut targets = serde_json::Map::new();
    targets.insert(target.to_string(), Value::Object(binaries));
    json!({
        "release": release,
        "version": version,
        "assets": targets,
    })
}

fn fixture_asset(url: String, sha256: String, bytes: u64, reports: Option<&str>) -> Value {
    json!({"url": url, "sha256": sha256, "bytes": bytes, "reports": reports})
}

// Used only by the setup-apply tests, which are gated off Windows above.
#[cfg(not(windows))]
fn fixture_archive(_root: &Path, binary: &str, version_line: &str, filler_bytes: u64) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho '{version_line}'\n").into_bytes();
    let filler = vec![0; filler_bytes.try_into().unwrap()];
    let archived_binary = platform_binary(binary);
    stored_zip(&[
        (archived_binary.as_str(), script.as_slice()),
        ("fixture-padding", filler.as_slice()),
    ])
}

// Used only by the setup-apply tests, which are gated off Windows above.
#[cfg(not(windows))]
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let offset = u32::try_from(archive.len()).unwrap();
        let size = u32::try_from(bytes.len()).unwrap();
        let crc = crc32(bytes);
        push_u32(&mut archive, 0x0403_4b50);
        push_u16(&mut archive, 20);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u32(&mut archive, crc);
        push_u32(&mut archive, size);
        push_u32(&mut archive, size);
        push_u16(&mut archive, u16::try_from(name.len()).unwrap());
        push_u16(&mut archive, 0);
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(bytes);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, u16::try_from(name.len()).unwrap());
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(archive.len()).unwrap();
    let central_size = u32::try_from(central.len()).unwrap();
    archive.extend_from_slice(&central);
    push_u32(&mut archive, 0x0605_4b50);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, u16::try_from(entries.len()).unwrap());
    push_u16(&mut archive, u16::try_from(entries.len()).unwrap());
    push_u32(&mut archive, central_size);
    push_u32(&mut archive, central_offset);
    push_u16(&mut archive, 0);
    archive
}

// Used only by the setup-apply tests, which are gated off Windows above.
#[cfg(not(windows))]
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

// Used only by the setup-apply tests, which are gated off Windows above.
#[cfg(not(windows))]
fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

// Used only by the setup-apply tests, which are gated off Windows above.
#[cfg(not(windows))]
fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_executable(path: &Path, contents: &str) {
    #[cfg(unix)]
    {
        use std::io::Write;
        let mut child = Command::new("sh")
            .args(["-c", "cat > \"$1\"", "write_executable"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("start executable fixture writer");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
    #[cfg(not(unix))]
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(unix)]
fn assert_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;

    let actual = fs::metadata(path)
        .expect("mode metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(actual, expected, "unexpected mode for {}", path.display());
}

fn service_manager_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchctl"
    } else if cfg!(windows) {
        "schtasks.exe"
    } else {
        "systemctl"
    }
}

fn service_manager_script() -> &'static str {
    "#!/bin/sh\necho 'state = running'\nexit 0\n"
}

fn runtime_definition(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/cortexkit.subc.plist")
    } else if cfg!(windows) {
        home.join("cortexkit-subc-daemon.xml")
    } else {
        home.join(".config/systemd/user/cortexkit-subc.service")
    }
}

fn platform_binary(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn host_target() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64".to_string(),
        ("linux", "x86_64") => "linux-x64".to_string(),
        ("linux", "aarch64") => "linux-arm64".to_string(),
        ("windows", "x86_64") => "windows-x64".to_string(),
        ("windows", "aarch64") => "windows-arm64".to_string(),
        (os, arch) => format!("{os}-{arch}"),
    }
}

fn single_row_table<const N: usize>(headers: [&str; N], row: [&str; N]) -> String {
    let widths: Vec<_> = headers
        .iter()
        .zip(row)
        .map(|(header, cell)| header.len().max(cell.len()))
        .collect();
    let render = |cells: [&str; N]| {
        cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ")
            + "\n"
    };
    format!("{}{}", render(headers), render(row))
}

/// Asserts `output` renders the daemon uptime taken at some instant while the
/// command ran. The CLI reads the uptime mid-run, so a test that samples it
/// once, before or after, fails whenever a whole second ticks over in
/// between. Sampling both sides bounds the value the CLI can have printed.
fn assert_rendered_with_uptime(
    output: &Output,
    before: &str,
    after: &str,
    render: impl Fn(&str) -> String,
) {
    let actual = text(&output.stdout);
    assert!(
        actual == render(before) || actual == render(after),
        "rendered output matched neither uptime sample ({before}, {after}):\n{actual}"
    );
}

fn connection_file_elapsed(path: &Path) -> String {
    let seconds = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn age_from_ms(timestamp_ms: u64) -> String {
    let now = now_ms();
    if timestamp_ms > now {
        let seconds = (timestamp_ms - now) / 1_000;
        return if seconds == 0 {
            "just now".to_string()
        } else {
            format!("in {}", compact_duration(seconds))
        };
    }
    let seconds = (now - timestamp_ms) / 1_000;
    if seconds == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", compact_duration(seconds))
    }
}

fn compact_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
#[cfg(unix)]
fn setup_core_drift_floor_refusal_is_actionable_and_never_mutates() {
    let fixture = SetupFixture::installed("setup-drift-floor");
    let binary_home = fixture.data_home.join("cortexkit/bin");
    fs::remove_file(binary_home.join("ck-aft")).unwrap();
    write_executable(
        &binary_home.join("ck-subc"),
        "#!/bin/sh\necho 'ck-subc 0.17.19'\n",
    );
    write_executable(
        &fixture.tools.join(service_manager_program()),
        "#!/bin/sh\nexit 1\n",
    );
    let config_path = fixture.config_home.join("cortexkit/subc.jsonc");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("storage");
    config["modules"].as_object_mut().unwrap().remove("aft");
    let config_bytes = serde_json::to_vec_pretty(&config).unwrap();
    fs::write(&config_path, &config_bytes).unwrap();
    let manifest_path = fixture.data_home.join("cortexkit/installer-manifest.json");
    let manifest_before = fs::read(&manifest_path).unwrap();
    let index = serve_signed_index(|base| {
        let (mut index, assets) = setup_index(base, None);
        // Keep the core release current so the module-floor refusal, rather
        // than the independent downgrade refusal, is the property under test.
        index["components"]["core"]["version"] = json!("0.17.19");
        index["components"]["core"]["release"] = json!("subc-core-v0.17.19");
        let mut aft = index_component(
            "v1.0.0",
            Some("1.0.0"),
            &host_target(),
            [(
                "ck-aft",
                fixture_asset(
                    format!("{base}/unused.zip"),
                    "00".repeat(32),
                    1,
                    Some("1.0.0"),
                ),
            )],
        );
        aft["requires_core"] = json!("0.17.20");
        index["components"]["aft"] = aft;
        (index, assets)
    });
    for args in [&["setup", "aft", "--dry-run"][..], &["setup", "aft"][..]] {
        let output = fixture
            .command(&index, args)
            .env_remove("CK_TEST_SETUP_CONTROL_OK")
            // This test is about the core-version floor, not the Linux user-manager
            // preflight, which a CI container cannot satisfy.
            .env("CK_TEST_SETUP_PREFLIGHT_OK", "1")
            .output()
            .unwrap();
        assert_exit(&output, 1);
        assert_eq!(
            text(&output.stdout),
            "aft requires core ≥ 0.17.20, installed 0.17.19; run `ck upgrade` first\n"
        );
        assert_eq!(fs::read(&config_path).unwrap(), config_bytes);
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
        assert!(!binary_home.join("ck-aft").exists());
    }
}

#[test]
#[cfg(unix)]
fn setup_unowned_refusals_match_dry_run_without_downloads_or_changes() {
    for version in ["0.20.98", "0.17.8", "unknown"] {
        let fixture = SetupFixture::fresh("setup-unowned-refusal");
        let binary_home = fixture.data_home.join("cortexkit/bin");
        let binary = binary_home.join("ck-subc-mcp");
        let script = format!("#!/bin/sh\necho 'ck-subc-mcp {version}'\n");
        write_executable(&binary, &script);
        let index = serve_signed_index(|base| setup_index(base, None));
        let mut outputs = Vec::new();
        for args in [&["setup", "--dry-run"][..], &["setup"][..]] {
            let output = fixture.command(&index, args).output().unwrap();
            assert_exit(&output, 1);
            assert!(!text(&output.stdout).contains("Installing"));
            assert!(!text(&output.stdout).contains("Continue?"));
            outputs.push(text(&output.stdout));
            assert_eq!(fs::read(&binary).unwrap(), script.as_bytes());
            assert!(!binary_home.join("ck-subc").exists());
            assert!(!fixture
                .data_home
                .join("cortexkit/installer-manifest.json")
                .exists());
            assert!(!fixture.config_home.join("cortexkit/subc.jsonc").exists());
            assert!(!runtime_definition(&fixture.home).exists());
        }
        assert_eq!(outputs[0], outputs[1]);
        if version == "0.20.98" {
            assert!(outputs[0].contains("0.20.98, newer than the latest release (0.17.9)"));
            assert!(outputs[0].contains("ck setup will not downgrade it. Nothing was changed."));
        } else {
            assert!(outputs[0]
                .contains("ck setup did not install ~/.local/share/cortexkit/bin/ck-subc-mcp, so it won't replace it. Move it aside and run ck setup again."));
        }
        assert_eq!(
            *index.requests.lock().unwrap(),
            vec!["/index.json", "/index.json"]
        );
    }
}

#[test]
#[cfg(unix)]
fn setup_owned_newer_daemon_refuses_even_when_configuration_is_correct() {
    let fixture = SetupFixture::installed("setup-owned-newer-daemon");
    write_executable(
        &fixture.data_home.join("cortexkit/bin/ck-subc"),
        "#!/bin/sh\necho 'ck-subc 0.20.98+local'\n",
    );
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let inventory = fixture.data_home.join("cortexkit/installer-manifest.json");
    let before = (fs::read(&config).unwrap(), fs::read(&inventory).unwrap());
    let index = serve_signed_index(|base| setup_index(base, None));
    for args in [&["setup", "--dry-run"][..], &["setup"][..]] {
        let output = fixture.command(&index, args).output().unwrap();
        assert_exit(&output, 1);
        assert_eq!(text(&output.stdout), "This computer runs CortexKit daemon 0.20.98+local, newer than the latest release (0.17.9). ck setup will not downgrade it. Nothing was changed.\n");
        assert_eq!(
            (fs::read(&config).unwrap(), fs::read(&inventory).unwrap()),
            before
        );
    }
    assert_eq!(
        *index.requests.lock().unwrap(),
        vec!["/index.json", "/index.json"]
    );
}

#[test]
fn daemon_build_info_is_offline_and_reports_the_core_release() {
    let root = TempDir::new("daemon-offline-build-info");
    let config_home = root.join("config");
    fs::create_dir_all(config_home.join("cortexkit")).unwrap();
    let config = config_home.join("cortexkit/subc.jsonc");
    fs::write(&config, "not valid configuration").unwrap();
    let runtime = root.join("runtime");
    let output = Command::new(cortexkit_test_support::ckdev_binary(env!(
        "CARGO_BIN_EXE_ck-subc-under-test"
    )))
    .arg("--build-info")
    .env("HOME", root.path())
    .env("USERPROFILE", root.path())
    .env("XDG_CONFIG_HOME", &config_home)
    .env("XDG_DATA_HOME", "relative-data-home-must-not-be-read")
    .env("XDG_RUNTIME_DIR", &runtime)
    .env("SUBC_CGROUP_PLACEMENT", "invalid-must-not-be-read")
    .output()
    .unwrap();
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    let info: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["core_version"], env!("CARGO_PKG_VERSION"));
    for (key, length) in [("build_git_sha", 40), ("build_lock_digest", 64)] {
        let value = info.get(key).expect("offline identity field");
        assert!(
            value.is_null()
                || value.as_str().is_some_and(|value| value.len() == length
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        );
    }
    assert_eq!(
        fs::read_to_string(config).unwrap(),
        "not valid configuration"
    );
    assert!(!runtime.exists());
    assert!(!root.join(".local").exists());
}

#[test]
#[cfg(unix)]
fn setup_live_daemon_uses_core_release_not_connection_library_version() {
    use std::os::unix::fs::PermissionsExt;
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint};

    let fixture = SetupFixture::installed("setup-live-core-version");
    write_executable(
        &fixture.data_home.join("cortexkit/bin/ck-subc"),
        "#!/bin/sh\necho 'ck-subc 0.20.83'\n",
    );
    set_setup_managed_modes(&fixture);
    let connection_path = fixture.home.join("live-daemon.json");
    let connection = ConnectionInfo {
        schema: connection_file::SCHEMA_VERSION,
        wire_version: Some(subc_protocol::PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port: 1,
        }],
        key: vec![7; connection_file::KEY_LEN],
        daemon_id: [8; connection_file::DAEMON_ID_LEN],
        pid: std::process::id(),
        daemon_ver: "0.36.5".to_string(),
    };
    fs::write(&connection_path, serde_json::to_vec(&connection).unwrap()).unwrap();
    fs::set_permissions(&connection_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        connection_file::discover(Some(&connection_path))
            .unwrap()
            .info
            .daemon_ver,
        "0.36.5"
    );
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let inventory = fixture.data_home.join("cortexkit/installer-manifest.json");
    let before = (fs::read(&config).unwrap(), fs::read(&inventory).unwrap());
    let index = serve_signed_index(|base| {
        let (mut index, assets) = setup_index(base, None);
        index["components"]["core"]["version"] = json!("0.20.83");
        index["components"]["core"]["release"] = json!("subc-core-v0.20.83");
        (index, assets)
    });
    for args in [&["setup", "--dry-run"][..], &["setup"][..]] {
        let output = fixture
            .command(&index, args)
            .env("SUBC_CONNECTION_FILE", &connection_path)
            .output()
            .unwrap();
        assert_exit(&output, 0);
        assert_eq!(text(&output.stdout), "CortexKit is set up: daemon running, aft · claustrum · insula ok.\nOptional modules not installed: mc, synapse — run `ck setup mc` or `ck setup synapse`.\n");
        assert_eq!(
            (fs::read(&config).unwrap(), fs::read(&inventory).unwrap()),
            before
        );
    }
    assert_eq!(
        *index.requests.lock().unwrap(),
        vec!["/index.json", "/index.json"]
    );
}

#[test]
fn setup_nothing_to_do_never_renders_outcome_no_op() {
    let fixture = SetupFixture::installed("ck-setup-current");
    // The no-op claim applies only when this existing install already has
    // setup's managed modes; loose paths now produce a real tightening step.
    #[cfg(unix)]
    set_setup_managed_modes(&fixture);
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture.command(&index, &["setup"]).output().unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "CortexKit is set up: daemon running, aft · claustrum · insula ok.\nOptional modules not installed: mc, synapse — run `ck setup mc` or `ck setup synapse`.\n"
    );
}

/// Places the test CLI in the fixture's `cmd` folder, where the bootstrap
/// installer puts `ck`, and records it in the manifest as setup's own. It
/// keeps its `ckdev-` name, so no test process runs under a production name;
/// `ckni` is placed beside it all the same.
fn place_managed_ck(fixture: &SetupFixture) -> PathBuf {
    let cortexkit = fixture.data_home.join("cortexkit");
    let cmd = cortexkit.join("cmd");
    fs::create_dir_all(&cmd).unwrap();
    let published = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-under-test"));
    let placed = cmd.join(published.file_name().unwrap());
    fs::copy(&published, &placed).unwrap();
    // Setup identifies the running ck by its canonical path.
    let ck = fs::canonicalize(&placed).unwrap();
    let manifest_path = cortexkit.join("installer-manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["mutations"].as_array_mut().unwrap().push(json!({
        "kind": "managed-binary",
        "path": ck,
        "component": "core",
    }));
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    ck
}

fn manifest_rows(fixture: &SetupFixture, kind: &str) -> Vec<Value> {
    let manifest: Value = serde_json::from_slice(
        &fs::read(fixture.data_home.join("cortexkit/installer-manifest.json")).unwrap(),
    )
    .unwrap();
    manifest["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == kind)
        .cloned()
        .collect()
}

#[test]
fn setup_puts_ckni_beside_the_managed_ck_and_then_has_nothing_to_do() {
    let fixture = SetupFixture::installed("ck-setup-ckni");
    #[cfg(unix)]
    set_setup_managed_modes(&fixture);
    let ck = place_managed_ck(&fixture);
    let ckni = ck.with_file_name(if cfg!(windows) { "ckni.exe" } else { "ckni" });
    let index = serve_signed_index(|base| setup_index(base, None));
    let run = |args: &[&str]| {
        fixture
            .command_from(Command::new(&ck), &index, args)
            .output()
            .unwrap()
    };
    let (verb, done) = if cfg!(windows) {
        ("would copy ck to ckni at ", "  copied ck to ")
    } else {
        ("would link ckni to ck at ", "  linked ")
    };

    let preview = run(&["setup", "--dry-run"]);
    assert_exit(&preview, 0);
    assert!(
        text(&preview.stdout).contains(verb),
        "stdout: {}",
        text(&preview.stdout)
    );
    assert!(
        fs::symlink_metadata(&ckni).is_err(),
        "a dry run placed ckni"
    );

    let applied = run(&["setup"]);
    assert_exit(&applied, 0);
    assert!(
        text(&applied.stdout).contains(done),
        "stdout: {}",
        text(&applied.stdout)
    );
    assert_eq!(fs::read(&ckni).unwrap(), fs::read(&ck).unwrap());
    let rows = manifest_rows(&fixture, "managed-link");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()), ckni);
    #[cfg(unix)]
    {
        assert_eq!(
            fs::read_link(&ckni).unwrap(),
            Path::new(ck.file_name().unwrap())
        );
        assert_eq!(rows[0]["mechanism"], "symlink");
    }
    #[cfg(windows)]
    {
        assert!(fs::symlink_metadata(&ckni).unwrap().file_type().is_file());
        assert_eq!(rows[0]["mechanism"], "copy");
    }
    // Setup touched nothing else in ck's folder.
    let mut names = fs::read_dir(ck.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    let mut expected = vec![
        ck.file_name().unwrap().to_string_lossy().into_owned(),
        ckni.file_name().unwrap().to_string_lossy().into_owned(),
    ];
    expected.sort();
    assert_eq!(names, expected);

    let again = run(&["setup"]);
    assert_exit(&again, 0);
    assert_eq!(
        text(&again.stdout),
        "CortexKit is set up: daemon running, aft · claustrum · insula ok.\nOptional modules not installed: mc, synapse — run `ck setup mc` or `ck setup synapse`.\n"
    );
}

/// Run from a byte copy named `ckni` (how Windows places it; on Unix a
/// regular file named `ckni` also keeps that name when its path is
/// canonicalized, unlike a symlink), setup acts on the `ck` beside the copy:
/// the plan adopts `ck`, the installer's unowned placement, rather than the
/// `ckni` it was started as. A setup that took the copy for `ck` would adopt
/// the copy and leave `ck` unmanaged, and an upgrade would replace the copy
/// instead.
#[test]
fn setup_run_from_a_ckni_copy_acts_on_the_ck_beside_it() {
    let fixture = SetupFixture::installed("ck-setup-from-ckni-copy");
    #[cfg(unix)]
    set_setup_managed_modes(&fixture);
    let cortexkit = fixture.data_home.join("cortexkit");
    let cmd = cortexkit.join("cmd");
    fs::create_dir_all(&cmd).unwrap();
    let published = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-under-test"));
    let (ck_name, ckni_name) = if cfg!(windows) {
        ("ck.exe", "ckni.exe")
    } else {
        ("ck", "ckni")
    };
    // `ck` is a placement the installer made and setup has not adopted yet.
    // Nothing executes it in a dry run; only the `ckni` copy runs.
    fs::copy(&published, cmd.join(ck_name)).unwrap();
    fs::copy(&published, cmd.join(ckni_name)).unwrap();
    let ck = fs::canonicalize(cmd.join(ck_name)).unwrap();
    let ckni = fs::canonicalize(cmd.join(ckni_name)).unwrap();
    // The copy is on record as setup's own, so it is not refused as unowned.
    let manifest_path = cortexkit.join("installer-manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["mutations"].as_array_mut().unwrap().push(json!({
        "kind": "managed-link",
        "path": ckni,
        "links_to": ck,
        "mechanism": "copy",
        "sha256": format!("{:x}", Sha256::digest(fs::read(&ckni).unwrap())),
    }));
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));

    let output = fixture
        .command_from(Command::new(&ckni), &index, &["setup", "--dry-run"])
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let adopted: Vec<&str> = stdout
        .lines()
        .filter_map(|line| {
            line.split_once(". would adopt ")?
                .1
                .strip_suffix(" as the managed ck binary")
        })
        .collect();
    assert_eq!(
        adopted.len(),
        1,
        "the plan adopts exactly one ck:\n{stdout}"
    );
    assert_eq!(
        Path::new(adopted[0]).file_name().unwrap(),
        ck_name,
        "setup run from the ckni copy adopted {} instead of {}:\n{stdout}",
        adopted[0],
        ck.display()
    );
    // The adopted path is that ck, not a ck elsewhere with the same name.
    let relative = cmd.join(ck_name);
    let relative = relative
        .strip_prefix(&fixture.home)
        .unwrap()
        .display()
        .to_string();
    assert!(
        adopted[0].ends_with(&relative),
        "{} is not {relative}",
        adopted[0]
    );
}

#[test]
fn setup_refuses_a_ckni_it_did_not_install_and_changes_nothing() {
    let fixture = SetupFixture::installed("ck-setup-ckni-unowned");
    #[cfg(unix)]
    set_setup_managed_modes(&fixture);
    let ck = place_managed_ck(&fixture);
    let ckni = ck.with_file_name(if cfg!(windows) { "ckni.exe" } else { "ckni" });
    fs::write(&ckni, b"someone else's ckni").unwrap();
    let manifest = fs::read(fixture.data_home.join("cortexkit/installer-manifest.json")).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));

    let output = fixture
        .command_from(Command::new(&ck), &index, &["setup"])
        .output()
        .unwrap();
    assert_exit(&output, 1);
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("ck setup did not install ")
            && stdout.ends_with(&format!(
                "{}, so it won't replace it. Move it aside and run ck setup again. Nothing was changed.\n",
                ckni.file_name().unwrap().to_string_lossy()
            )),
        "stdout: {stdout}"
    );
    assert_eq!(fs::read(&ckni).unwrap(), b"someone else's ckni");
    assert_eq!(
        fs::read(fixture.data_home.join("cortexkit/installer-manifest.json")).unwrap(),
        manifest
    );
}

#[cfg(unix)]
fn set_setup_managed_modes(fixture: &SetupFixture) {
    use std::os::unix::fs::PermissionsExt;

    let cortexkit = fixture.data_home.join("cortexkit");
    let binary_dir = cortexkit.join("bin");
    let config = fixture.config_home.join("cortexkit/subc.jsonc");
    let config_dir = config.parent().unwrap().to_path_buf();
    let manifest = cortexkit.join("installer-manifest.json");
    for (path, mode) in [
        (&cortexkit, 0o700),
        (&binary_dir, 0o755),
        (&config_dir, 0o700),
        (&config, 0o600),
        (&manifest, 0o644),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    for binary in [
        "ck-subc",
        "ck-subc-mcp",
        "ck-aft",
        "ck-claustrum",
        "ck-auth",
        "ck-insula",
    ] {
        fs::set_permissions(
            cortexkit.join("bin").join(platform_binary(binary)),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

// Magic Context has no Windows release (`Component::Mc` is declared
// unsupported there, and the served index carries no Windows asset for it),
// so on Windows `ck setup mc` refuses before any of the steps these two tests
// pin; the refusal itself is covered by the planner's unit tests.
#[cfg(not(windows))]
#[test]
fn setup_mc_apply_reports_each_completed_change_without_a_plan() {
    let fixture = SetupFixture::installed("ck-setup-mc-apply");
    let archive = fixture_archive(
        fixture._root.path(),
        "ck-mc",
        "ck-mc ck-mc-alpha.22464bf2",
        12 * 1024 * 1024 + 102 * 1024,
    );
    let index = serve_signed_index(|base| setup_index(base, Some(&archive)));
    let output = fixture.command(&index, &["setup", "mc"]).output().unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Installing magic-context (ck-mc-alpha.22464bf2, {})\n  downloaded and verified ck-mc (12.1 MiB)\n  placed ~/.local/share/cortexkit/bin/{}\n  configured magic-context in ~/.config/cortexkit/subc.jsonc\n  registered with the daemon; magic-context ok\nDone.\n",
            host_target(),
            platform_binary("ck-mc")
        )
    );
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("setup plan:"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("proposed configuration diff:"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn setup_definition_command_matches_inventory_binary_path() {
    let fixture = SetupFixture::installed("ck-definition-command");
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .data_home
                .join("cortexkit")
                .join("installer-manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let binary = manifest["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["kind"] == "managed-binary"
                && entry["path"].as_str().is_some_and(|path| {
                    Path::new(path)
                        .file_name()
                        .is_some_and(|name| name == platform_binary("ck-subc").as_str())
                })
        })
        .expect("owned daemon binary");
    let expected = binary["path"].as_str().unwrap();
    for outdated in [false, true] {
        fixture.write_runtime_definition(outdated);
        let text = fs::read_to_string(runtime_definition(&fixture.home)).unwrap();
        let actual = if cfg!(windows) || cfg!(target_os = "macos") {
            let document = roxmltree::Document::parse(&text).unwrap();
            let command = if cfg!(windows) {
                document.descendants().find(|node| {
                    node.has_tag_name((
                        "http://schemas.microsoft.com/windows/2004/02/mit/task",
                        "Command",
                    ))
                })
            } else {
                document
                    .descendants()
                    .find(|node| node.has_tag_name("array"))
                    .and_then(|node| node.children().find(|node| node.has_tag_name("string")))
            }
            .expect("service executable element");
            command.text().unwrap().to_string()
        } else {
            text.lines()
                .find_map(|line| line.strip_prefix("ExecStart=\""))
                .unwrap()
                .strip_suffix('"')
                .unwrap()
                .to_string()
        };
        assert_eq!(actual, expected, "the service command must spell the inventory-owned daemon path identically (outdated={outdated})");
    }
}

#[test]
fn setup_dry_run_shows_owned_service_definition_update_without_writing() {
    let fixture = SetupFixture::installed("ck-setup-definition-dry-run");
    fixture.write_runtime_definition(true);
    let definition = runtime_definition(&fixture.home);
    let before = fs::read(&definition).unwrap();
    let inventory = fixture.data_home.join("cortexkit/installer-manifest.json");
    let inventory_before = fs::read(&inventory).unwrap();
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command(&index, &["setup", "--dry-run"])
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("would update the daemon service definition"),
        "{stdout}"
    );
    assert!(stdout.contains("restart after a crash"), "{stdout}");
    if cfg!(target_os = "macos") {
        assert!(
            stdout.contains("restarts the daemon and every module"),
            "{stdout}"
        );
    }
    assert_eq!(fs::read(&definition).unwrap(), before);
    assert_eq!(fs::read(&inventory).unwrap(), inventory_before);
}

#[cfg(not(windows))]
#[test]
fn setup_mc_dry_run_has_future_steps_and_the_config_diff() {
    let fixture = SetupFixture::installed("ck-setup-mc-dry-run");
    let archive = fixture_archive(
        fixture._root.path(),
        "ck-mc",
        "ck-mc ck-mc-alpha.22464bf2",
        12 * 1024 * 1024 + 102 * 1024,
    );
    let index = serve_signed_index(|base| setup_index(base, Some(&archive)));
    let output = fixture
        .command(&index, &["setup", "mc", "--dry-run"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for line in [
        "  1. would tighten permissions on existing CortexKit paths".to_string(),
        "  2. would download and verify ck-mc (12.1 MiB)".to_string(),
        format!(
            "  3. would place ~/.local/share/cortexkit/bin/{}",
            platform_binary("ck-mc")
        ),
        "  4. would configure magic-context in ~/.config/cortexkit/subc.jsonc".to_string(),
        "  5. would register magic-context with the daemon and check its health".to_string(),
        "proposed configuration diff:".to_string(),
        "+    \"magic-context\": {".to_string(),
    ] {
        assert!(
            stdout.contains(&line),
            "missing {line:?} in stdout:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("observe alpha platform support"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("validate with ck daemon triage"),
        "{stdout}"
    );
}

#[test]
fn setup_synapse_without_a_host_asset_is_one_complete_refusal_line() {
    let fixture = SetupFixture::installed("ck-setup-synapse-unavailable");
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command(&index, &["setup", "synapse"])
        .output()
        .unwrap();

    assert_exit(&output, 1);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "synapse has no {} release yet; nothing was installed.\n",
            host_target()
        )
    );
}

#[test]
fn setup_verbose_keeps_every_preexisting_outcome_line() {
    let fixture = SetupFixture::installed("ck-setup-verbose");
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command(&index, &["setup", "--verbose"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for outcome in [
        "  outcome: no-op: core is already correct",
        "  outcome: no-op: per-user runtime is already registered and live",
    ] {
        assert!(stdout.contains(outcome), "missing {outcome:?}:\n{stdout}");
    }
}

// The apply path registers and starts the daemon through the service
// manager, which these tests stand in for with a script on PATH. Windows
// process creation searches System32 before PATH, so a fixture named
// `schtasks.exe` can never shadow the real one and the real one refuses the
// task it cannot find. The Windows registration path is exercised by the
// setup-runtime CI job against a real scheduled task instead.
#[cfg(not(windows))]
#[test]
fn fresh_setup_prints_the_pasteable_claude_code_command() {
    let fixture = SetupFixture::fresh("ck-setup-fresh-next");
    let subc = fixture_archive(fixture._root.path(), "ck-subc", "ck-subc 0.17.10", 0);
    let mcp = fixture_archive(
        fixture._root.path(),
        "ck-subc-mcp",
        "ck-subc-mcp 0.17.10",
        0,
    );
    let index = serve_signed_index(|base| {
        let target = host_target();
        let mut assets = BTreeMap::new();
        assets.insert("/ck-subc.zip".to_string(), subc.clone());
        assets.insert("/ck-subc-mcp.zip".to_string(), mcp.clone());
        (
            json!({
                "schema": 1,
                "channel": "alpha",
                "generated_at_ms": now_ms(),
                "components": {
                    "core": index_component(
                        "subc-core-v0.17.10",
                        Some("0.17.10"),
                        &target,
                        [
                            ("ck-subc", fixture_asset(format!("{base}/ck-subc.zip"), sha256_hex(&subc), subc.len() as u64, Some("0.17.10"))),
                            ("ck-subc-mcp", fixture_asset(format!("{base}/ck-subc-mcp.zip"), sha256_hex(&mcp), mcp.len() as u64, Some("0.17.10"))),
                        ],
                    ),
                },
            }),
            assets,
        )
    });
    let output = fixture
        .command(&index, &["setup"])
        .env("CK_TEST_SETUP_RUNTIME_MISSING", "1")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.ends_with(
            "next: connect your agent — Claude Code: claude mcp add ck -- ck subc-mcp shim --harness claude-code\n      other harnesses: https://github.com/cortexkit/subconscious#readme\n"
        ),
        "stdout:\n{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn setup_creates_owned_paths_with_explicit_modes_under_zero_umask() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = SetupFixture::fresh("ck-setup-zero-umask-modes");
    let cortexkit = fixture.data_home.join("cortexkit");
    let bin = cortexkit.join("bin");
    fs::set_permissions(&cortexkit, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o777)).unwrap();
    let stage_report = fixture._root.path().join("stage-modes");
    write_executable(
        &fixture.tools.join("unzip"),
        r##"#!/bin/sh
set -eu
archive=""
destination=""
while [ $# -gt 0 ]; do
  case "$1" in
    -q) shift ;;
    -d) destination="$2"; shift 2 ;;
    *) archive="$1"; shift ;;
  esac
done
mode_of() {
  local mode
  mode=$(stat -f '%Lp' "$1" 2>/dev/null) || mode=$(stat -c '%a' "$1")
  printf '%s' "$mode"
}
printf '%s %s %s\n' "$(mode_of "$(dirname "$archive")")" "$(mode_of "$destination")" "$(mode_of "$archive")" >>"$CK_TEST_STAGE_REPORT"
case "$archive" in
  *ck-subc-mcp-*.zip) binary=ck-subc-mcp ;;
  *ck-subc-*.zip) binary=ck-subc ;;
  *) exit 2 ;;
esac
printf '#!/bin/sh\necho "%s 0.17.10"\n' "$binary" >"$destination/$binary"
"##,
    );

    let subc = fixture_archive(fixture._root.path(), "ck-subc", "ck-subc 0.17.10", 0);
    let mcp = fixture_archive(
        fixture._root.path(),
        "ck-subc-mcp",
        "ck-subc-mcp 0.17.10",
        0,
    );
    let index = serve_signed_index(|base| {
        let target = host_target();
        let mut assets = BTreeMap::new();
        assets.insert("/ck-subc.zip".to_string(), subc.clone());
        assets.insert("/ck-subc-mcp.zip".to_string(), mcp.clone());
        (
            json!({
                "schema": 1,
                "channel": "alpha",
                "generated_at_ms": now_ms(),
                "components": {
                    "core": index_component(
                        "subc-core-v0.17.10",
                        Some("0.17.10"),
                        &target,
                        [
                            ("ck-subc", fixture_asset(format!("{base}/ck-subc.zip"), sha256_hex(&subc), subc.len() as u64, Some("0.17.10"))),
                            ("ck-subc-mcp", fixture_asset(format!("{base}/ck-subc-mcp.zip"), sha256_hex(&mcp), mcp.len() as u64, Some("0.17.10"))),
                        ],
                    ),
                },
            }),
            assets,
        )
    });
    let output = fixture
        .command_with_zero_umask(&index, &["setup"])
        .env("CK_TEST_SETUP_RUNTIME_MISSING", "1")
        .env("CK_TEST_STAGE_REPORT", &stage_report)
        .output()
        .expect("setup runs with zero umask in its child");

    assert_exit(&output, 0);
    assert_mode(&cortexkit, 0o700);
    assert_mode(&bin, 0o755);
    assert_mode(&bin.join(platform_binary("ck-subc")), 0o755);
    assert_mode(&bin.join(platform_binary("ck-subc-mcp")), 0o755);
    assert_mode(&cortexkit.join("installer-manifest.json"), 0o644);
    let config = fixture.config_home.join("cortexkit");
    assert_mode(&config, 0o700);
    assert_mode(&config.join("subc.jsonc"), 0o600);
    assert_mode(&runtime_definition(&fixture.home), 0o644);
    let stage_modes = fs::read_to_string(stage_report).expect("staging mode report");
    assert_eq!(stage_modes.lines().count(), 2, "{stage_modes}");
    for modes in stage_modes.lines() {
        assert_eq!(modes, "700 700 600", "unexpected staging modes: {modes}");
    }
}

#[cfg(unix)]
#[test]
fn setup_tightens_existing_owned_modes_on_a_noop_plan_without_chmodding_unowned_files() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = SetupFixture::installed("ck-setup-tighten-existing-modes");
    let cortexkit = fixture.data_home.join("cortexkit");
    let bin = cortexkit.join("bin");
    let config = fixture.config_home.join("cortexkit").join("subc.jsonc");
    let manifest = cortexkit.join("installer-manifest.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    document["mutations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind": "configuration", "path": config.clone()}));
    fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    for path in [&cortexkit, &bin] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
    }
    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o666)).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
    for binary in [
        "ck-subc",
        "ck-subc-mcp",
        "ck-aft",
        "ck-claustrum",
        "ck-auth",
        "ck-insula",
    ] {
        fs::set_permissions(
            bin.join(platform_binary(binary)),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
    }
    let unrelated = cortexkit.join("user-owned.txt");
    fs::write(&unrelated, "leave this file alone").unwrap();
    fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o666)).unwrap();

    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture.command_direct(&index, &["setup"]).output().unwrap();

    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "  tightened permissions on existing CortexKit paths\nDone.\n"
    );
    assert_mode(&cortexkit, 0o700);
    assert_mode(&bin, 0o755);
    assert_mode(&manifest, 0o644);
    assert_mode(&config, 0o600);
    assert_mode(&unrelated, 0o666);
    for binary in [
        "ck-subc",
        "ck-subc-mcp",
        "ck-aft",
        "ck-claustrum",
        "ck-auth",
        "ck-insula",
    ] {
        assert_mode(&bin.join(platform_binary(binary)), 0o755);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn setup_keeps_bootstrapped_claustrum_key_owner_only_under_zero_umask() {
    let fixture = SetupFixture::fresh("ck-setup-zero-umask-key");
    let core_daemon = fixture_archive(fixture._root.path(), "ck-subc", "ck-subc 0.17.10", 0);
    let core_mcp = fixture_archive(
        fixture._root.path(),
        "ck-subc-mcp",
        "ck-subc-mcp 0.17.10",
        0,
    );
    let claustrum = fixture_archive(
        fixture._root.path(),
        "ck-claustrum",
        "ck-claustrum 0.17.10",
        0,
    );
    let auth_binary = platform_binary("ck-auth");
    let auth_script = "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'ck-auth 0.17.10'; exit 0; fi\nif [ \"$1\" = \"bootstrap\" ]; then shift; while [ $# -gt 0 ]; do if [ \"$1\" = \"--key-path\" ]; then key=\"$2\"; shift 2; else shift; fi; done; if [ -n \"${key:-}\" ]; then printf secret >\"$key\"; fi; exit 0; fi\nexit 2\n".to_string();
    let auth = stored_zip(&[(&auth_binary, auth_script.as_bytes())]);
    let index = serve_signed_index(|base| {
        let target = host_target();
        let mut assets = BTreeMap::new();
        for (name, archive) in [
            ("ck-subc", &core_daemon),
            ("ck-subc-mcp", &core_mcp),
            ("ck-claustrum", &claustrum),
            ("ck-auth", &auth),
        ] {
            assets.insert(format!("/{name}.zip"), archive.clone());
        }
        (
            json!({
                "schema": 1,
                "channel": "alpha",
                "generated_at_ms": now_ms(),
                "components": {
                    "core": index_component(
                        "subc-core-v0.17.10",
                        Some("0.17.10"),
                        &target,
                        [
                            ("ck-subc", fixture_asset(format!("{base}/ck-subc.zip"), sha256_hex(&core_daemon), core_daemon.len() as u64, Some("0.17.10"))),
                            ("ck-subc-mcp", fixture_asset(format!("{base}/ck-subc-mcp.zip"), sha256_hex(&core_mcp), core_mcp.len() as u64, Some("0.17.10"))),
                        ],
                    ),
                    "claustrum": index_component(
                        "subc-claustrum-v0.17.10",
                        Some("0.17.10"),
                        &target,
                        [
                            ("ck-claustrum", fixture_asset(format!("{base}/ck-claustrum.zip"), sha256_hex(&claustrum), claustrum.len() as u64, Some("0.17.10"))),
                            ("ck-auth", fixture_asset(format!("{base}/ck-auth.zip"), sha256_hex(&auth), auth.len() as u64, Some("0.17.10"))),
                        ],
                    ),
                },
            }),
            assets,
        )
    });
    let output = fixture
        .command_with_zero_umask(&index, &["setup", "claustrum"])
        .env("CK_TEST_SETUP_RUNTIME_MISSING", "1")
        .output()
        .expect("claustrum setup runs with zero umask in its child");

    assert_exit(&output, 0);
    let cortexkit = fixture.data_home.join("cortexkit");
    assert_mode(&cortexkit, 0o700);
    assert_mode(&cortexkit.join("claustrum"), 0o700);
    assert_mode(&cortexkit.join("claustrum/master.key"), 0o600);
}

struct UpgradeFixture {
    _root: TempDir,
    home: PathBuf,
    connection_file: PathBuf,
    #[cfg(windows)]
    core_version: std::cell::RefCell<String>,
}

impl UpgradeFixture {
    fn new(name: &str) -> Self {
        let root = TempDir::new(name);
        let home = root.path().join("home");
        let cortexkit = home.join(".local/share/cortexkit");
        let bin = cortexkit.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let aft = bin.join(platform_binary("ck-aft"));
        let daemon = bin.join(platform_binary("ck-subc"));
        let mcp = bin.join(platform_binary("ck-subc-mcp"));
        let claustrum = bin.join(platform_binary("ck-claustrum"));
        let auth = bin.join(platform_binary("ck-auth"));
        write_executable(&aft, "#!/bin/sh\necho 'ck-aft 0.55.1'\n");
        write_executable(&daemon, "#!/bin/sh\necho 'ck-subc 0.17.9'\n");
        write_executable(&mcp, "#!/bin/sh\necho 'ck-subc-mcp 0.17.9'\n");
        write_executable(&claustrum, "#!/bin/sh\necho 'ck-claustrum 0.8.0'\n");
        write_executable(&auth, "#!/bin/sh\necho 'ck-auth 0.8.0'\n");
        // The manifest names the `ck` this fixture's commands run: the test
        // CLI as published under its `ckdev-` name.
        let ck = fs::canonicalize(cortexkit_test_support::ckdev_binary(env!(
            "CARGO_BIN_EXE_ck-under-test"
        )))
        .unwrap();
        let mutations = [
            (ck, "44".repeat(32)),
            (daemon, "33".repeat(32)),
            (mcp, "11".repeat(32)),
            (aft, "22".repeat(32)),
            (claustrum, "55".repeat(32)),
            (auth, "66".repeat(32)),
        ]
        .into_iter()
        .map(|(path, archive_sha256)| {
            json!({
                "kind": "managed-binary",
                "path": path,
                "archive_sha256": archive_sha256,
            })
        })
        .collect::<Vec<_>>();
        fs::write(
            cortexkit.join("installer-manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "platform": host_target(),
                "mutations": mutations,
            }))
            .unwrap(),
        )
        .unwrap();

        let connection_file = root.path().join("subc-connection.json");
        write_atomic(
            &connection_file,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".to_string(),
                    port: 9,
                }],
                key: generate_key().unwrap(),
                daemon_id: generate_daemon_id().unwrap(),
                pid: std::process::id(),
                daemon_ver: "0.36.5".to_string(),
            },
        )
        .unwrap();
        Self {
            _root: root,
            home,
            connection_file,
            #[cfg(windows)]
            core_version: std::cell::RefCell::new("0.17.9".to_string()),
        }
    }

    fn command(&self, index: &SignedIndex, args: &[&str]) -> Command {
        let mut command = ck_command();
        command
            .args(args)
            .arg("--subc")
            .arg(&self.connection_file)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join(".local/share"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_RUNTIME_DIR", self._root.path().join("runtime"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("CK_RELEASE_INDEX_URL", &index.url)
            .env("CK_TEST_RELEASE_INDEX_PUBKEY", &index.public_key)
            .env("CK_TEST_CK_VERSION", "0.17.9")
            .env("CK_TEST_SUBC_MCP_VERSION", "0.17.9")
            .env("CK_TEST_AFT_VERSION", "0.55.1")
            .env("CK_TEST_CLAUSTRUM_VERSION", "0.8.0")
            .env("CK_TEST_AUTH_VERSION", "0.8.0")
            .env("CK_TEST_SETUP_MODULES", "aft,claustrum")
            .env(
                "CK_UPDATE_CACHE_PATH",
                self._root.path().join("update-metadata.json"),
            );
        #[cfg(windows)]
        command.env("CK_TEST_SUBC_VERSION", self.core_version.borrow().as_str());
        command
    }

    fn set_core_version(&self, version: &str) {
        #[cfg(windows)]
        self.core_version.replace(version.to_string());
        write_executable(
            &self
                .home
                .join(".local/share/cortexkit/bin")
                .join(platform_binary("ck-subc")),
            &format!("#!/bin/sh\necho 'ck-subc {version}'\n"),
        );
        let path = self
            .home
            .join(".local/share/cortexkit/installer-manifest.json");
        let mut inventory: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        // The fixture records ck, ck-subc and ck-subc-mcp before other components.
        for entry in inventory["mutations"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .take(3)
        {
            entry["version"] = json!(version);
        }
        fs::write(path, serde_json::to_vec(&inventory).unwrap()).unwrap();
    }
}

fn upgrade_index(
    base: &str,
    aft_version: &str,
    aft_digest: &str,
    ck_version: &str,
    ck_digest: &str,
    claustrum_version: &str,
    claustrum_digest: &str,
) -> (Value, BTreeMap<String, Vec<u8>>) {
    let target = host_target();
    (
        json!({
            "schema": 1,
            "channel": "alpha",
            "generated_at_ms": now_ms(),
            "components": {
                "core": index_component(
                    "subc-core-v0.17.10",
                    Some(ck_version),
                    &target,
                    [
                        ("ck-subc-mcp", fixture_asset(format!("{base}/mcp.zip"), "11".repeat(32), 1, Some("0.17.9"))),
                        ("ck-subc", fixture_asset(format!("{base}/daemon.zip"), "33".repeat(32), 1, Some("0.17.9"))),
                        ("ck", fixture_asset(format!("{base}/ck.zip"), ck_digest.to_string(), 1, Some(ck_version))),
                    ],
                ),
                "aft": index_component(
                    "v0.55.2",
                    Some(aft_version),
                    &target,
                    [(
                        "ck-aft",
                        fixture_asset(format!("{base}/aft.zip"), aft_digest.to_string(), 1, Some(aft_version)),
                    )],
                ),
                "claustrum": index_component(
                    "v0.8.1",
                    Some(claustrum_version),
                    &target,
                    [
                        (
                            "ck-claustrum",
                            fixture_asset(
                                format!("{base}/claustrum.zip"),
                                claustrum_digest.to_string(),
                                1,
                                Some(claustrum_version),
                            ),
                        ),
                        (
                            "ck-auth",
                            fixture_asset(
                                format!("{base}/auth.zip"),
                                "66".repeat(32),
                                1,
                                Some(claustrum_version),
                            ),
                        ),
                    ],
                ),
            },
        }),
        BTreeMap::new(),
    )
}

#[test]
fn upgrade_and_check_say_everything_is_current_in_one_line() {
    let fixture = UpgradeFixture::new("ck-upgrade-current");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.1",
            &"22".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    let expected =
        "Everything is up to date (ck 0.17.9 · ck-subc 0.17.9 · ck-subc-mcp · ck-aft 0.55.1 · ck-claustrum 0.8.0 · ck-auth 0.8.0).\n";

    for args in [&["upgrade"][..], &["upgrade", "--check"][..]] {
        let output = fixture.command(&index, args).output().unwrap();
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
        assert_eq!(text(&output.stdout), expected);
    }
}

// These byte pins protect redirected output as the terminal UI evolves. The
// index offers and installed versions are fixed; timestamps and URLs never
// enter these outputs, so no clock or path is masked.
#[test]
#[cfg(target_os = "linux")]
fn upgrade_check_piped_pins() {
    for (available, plain, verbose) in [
        (
            false,
            include_str!("golden/piped/upgrade-check.txt"),
            include_str!("golden/piped/upgrade-check.verbose.txt"),
        ),
        (
            true,
            include_str!("golden/piped/upgrade-check.available.txt"),
            include_str!("golden/piped/upgrade-check.available.verbose.txt"),
        ),
    ] {
        let fixture = UpgradeFixture::new("upgrade-check-pin");
        let index = serve_signed_index(|base| {
            upgrade_index(
                base,
                if available { "0.55.2" } else { "0.55.1" },
                &if available { "aa" } else { "22" }.repeat(32),
                "0.17.9",
                &"44".repeat(32),
                "0.8.0",
                &"55".repeat(32),
            )
        });
        for (args, pin) in [
            (&["upgrade", "--check"][..], plain),
            (&["upgrade", "--check", "--verbose"][..], verbose),
        ] {
            let output = fixture.command(&index, args).output().unwrap();
            assert_piped_upgrade_pin(&output, pin);
        }
        assert_eq!(*index.requests.lock().unwrap(), ["/index.json"; 2]);
    }
}

#[cfg(target_os = "linux")]
fn assert_piped_upgrade_pin(output: &Output, pin: &str) {
    let captured = format!(
        "exit: {}\nstdout:\n{}stderr:\n{}",
        output.status.code().expect("ck exited without a signal"),
        text(&output.stdout),
        text(&output.stderr),
    );
    assert_eq!(captured, pin);
}

#[test]
#[cfg(target_os = "linux")]
fn setup_unowned_piped_pin() {
    let fixture = SetupFixture::fresh("setup-unowned-pin");
    let binary = fixture.data_home.join("cortexkit/bin/ck-subc-mcp");
    let bytes = "#!/bin/sh\necho 'ck-subc-mcp 0.17.8'\n";
    write_executable(&binary, bytes);
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture.command(&index, &["setup"]).output().unwrap();
    assert_piped_upgrade_pin(&output, include_str!("golden/piped/setup-unowned.txt"));
    assert_eq!(fs::read(binary).unwrap(), bytes.as_bytes());
    assert!(!fixture
        .data_home
        .join("cortexkit/installer-manifest.json")
        .exists());
    assert_eq!(*index.requests.lock().unwrap(), ["/index.json"]);
}

#[test]
#[cfg(target_os = "linux")]
fn upgrade_unowned_piped_pin() {
    let fixture = UpgradeFixture::new("upgrade-unowned-pin");
    let inventory = fixture
        .home
        .join(".local/share/cortexkit/installer-manifest.json");
    fs::remove_file(&inventory).unwrap();
    let binary = fixture.home.join(".local/share/cortexkit/bin/ck-aft");
    let bytes = fs::read(&binary).unwrap();
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    for args in [
        &["upgrade"][..],
        &["upgrade", "--check"][..],
        &["upgrade", "--verbose"][..],
        &["upgrade", "--check", "--verbose"][..],
    ] {
        let output = fixture.command(&index, args).output().unwrap();
        assert_piped_upgrade_pin(&output, include_str!("golden/piped/upgrade-unowned.txt"));
    }
    assert_eq!(fs::read(binary).unwrap(), bytes);
    assert!(!inventory.exists());
    assert!(index.requests.lock().unwrap().is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn upgrade_unowned_inline_pin() {
    let fixture = UpgradeFixture::new("upgrade-unowned-inline-pin");
    fs::remove_file(
        fixture
            .home
            .join(".local/share/cortexkit/installer-manifest.json"),
    )
    .unwrap();
    let index = serve_signed_index(|base| core_release_index(base, "0.17.9"));
    for args in [&["upgrade"][..], &["upgrade", "--verbose"][..]] {
        let command = fixture.command(&index, args);
        let output = upgrade_terminal_output(&command);
        assert_piped_upgrade_pin(&output, include_str!("golden/piped/upgrade-unowned.txt"));
    }
    assert!(index.requests.lock().unwrap().is_empty());
}

#[cfg(target_os = "linux")]
fn upgrade_terminal_output(command: &Command) -> Output {
    let quote =
        |word: &std::ffi::OsStr| format!("'{}'", word.to_str().unwrap().replace('\'', "'\\''"));
    let shell_command = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    // util-linux script supplies a real terminal and preserves the child's exit
    // code. Only the terminal driver's LF-to-CRLF translation is undone.
    let mut terminal = Command::new("script");
    terminal.args(["-qec", &shell_command, "/dev/null"]);
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            terminal.env(name, value);
        } else {
            terminal.env_remove(name);
        }
    }
    let mut output = terminal
        .output()
        .expect("util-linux script is required for the terminal pin");
    output.stdout = text(&output.stdout).replace("\r\n", "\n").into_bytes();
    output
}

#[test]
#[cfg(target_os = "linux")]
fn upgrade_without_managed_binaries_is_not_current() {
    for empty_inventory in [false, true] {
        let fixture = UpgradeFixture::new("upgrade-nothing-managed");
        let cortexkit = fixture.home.join(".local/share/cortexkit");
        fs::remove_dir_all(cortexkit.join("bin")).unwrap();
        let inventory = cortexkit.join("installer-manifest.json");
        if empty_inventory {
            fs::write(&inventory, br#"{"schema_version":1,"mutations":[]}"#).unwrap();
        } else {
            fs::remove_file(&inventory).unwrap();
        }
        let before = fs::read(&inventory).ok();
        let index = serve_signed_index(|base| core_release_index(base, "0.17.9"));
        for args in [
            &["upgrade"][..],
            &["upgrade", "--check"][..],
            &["upgrade", "--check", "--verbose"][..],
            &["upgrade", "--dry-run"][..],
        ] {
            let output = fixture.command(&index, args).output().unwrap();
            assert_piped_upgrade_pin(
                &output,
                include_str!("golden/piped/upgrade-nothing-managed.txt"),
            );
            assert_eq!(fs::read(&inventory).ok(), before);
        }
        assert!(index.requests.lock().unwrap().is_empty());
    }
}

#[test]
#[cfg(target_os = "linux")]
fn upgrade_mixed_ownership_refuses_before_checking_or_changing_binaries() {
    let fixture = UpgradeFixture::new("upgrade-mixed-ownership");
    let cortexkit = fixture.home.join(".local/share/cortexkit");
    let inventory = cortexkit.join("installer-manifest.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&inventory).unwrap()).unwrap();
    document["mutations"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| !entry["path"].as_str().unwrap().ends_with("ck-auth"));
    fs::write(&inventory, serde_json::to_vec(&document).unwrap()).unwrap();
    let before = fs::read(&inventory).unwrap();
    let binary = cortexkit.join("bin/ck-auth");
    let marker = fixture._root.path().join("unowned-was-executed");
    let bytes = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    write_executable(&binary, &bytes);
    let index = serve_signed_index(|base| core_release_index(base, "0.17.9"));
    for args in [
        &["upgrade", "--check"][..],
        &["upgrade"][..],
        &["upgrade", "--verbose"][..],
    ] {
        let output = fixture.command(&index, args).output().unwrap();
        assert_exit(&output, 1);
        assert!(output.stderr.is_empty());
        let reason = "ck upgrade manages only what ck setup installed. ck setup did not install these, so ck upgrade won't replace them:\n  ~/.local/share/cortexkit/bin/ck-auth\nMove them aside and run ck setup again. Nothing was changed.";
        let expected = format!("{reason}\n");
        assert_eq!(text(&output.stdout), expected);
        assert_eq!(fs::read(&inventory).unwrap(), before);
        assert_eq!(fs::read(&binary).unwrap(), bytes.as_bytes());
        assert!(
            !marker.exists(),
            "unowned discovery must not execute the binary"
        );
    }
    assert!(index.requests.lock().unwrap().is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn upgrade_refuses_an_unowned_running_cli_even_when_other_binaries_are_managed() {
    let fixture = UpgradeFixture::new("upgrade-unowned-running-cli");
    let inventory = fixture
        .home
        .join(".local/share/cortexkit/installer-manifest.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&inventory).unwrap()).unwrap();
    let cli = document["mutations"].as_array_mut().unwrap().remove(0)["path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(&inventory, serde_json::to_vec(&document).unwrap()).unwrap();
    let before = fs::read(&inventory).unwrap();
    let index = serve_signed_index(|base| core_release_index(base, "0.17.9"));
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .output()
        .unwrap();
    assert_exit(&output, 1);
    assert!(output.stderr.is_empty());
    assert_eq!(text(&output.stdout), format!("ck upgrade manages only what ck setup installed. ck setup did not install these, so ck upgrade won't replace them:\n  {cli}\nMove them aside and run ck setup again. Nothing was changed.\n"));
    assert_eq!(fs::read(&inventory).unwrap(), before);
    assert!(index.requests.lock().unwrap().is_empty());
}

fn core_release_index(base: &str, version: &str) -> (Value, BTreeMap<String, Vec<u8>>) {
    let (mut index, archives) = upgrade_index(
        base,
        "0.55.1",
        &"22".repeat(32),
        version,
        &"aa".repeat(32),
        "0.8.0",
        &"55".repeat(32),
    );
    let core = &mut index["components"]["core"];
    core["release"] = json!(format!("subc-core-v{version}"));
    let assets = &mut core["assets"][host_target()];
    assets["ck-subc"]["reports"] = json!(version);
    assets["ck-subc"]["sha256"] = json!("bb".repeat(32));
    assets["ck-subc-mcp"]["reports"] = Value::Null;
    assets["ck-subc-mcp"]["sha256"] = json!("cc".repeat(32));
    (index, archives)
}

#[test]
fn upgrade_noops_when_0_20_83_is_installed_and_cache_and_index_offer_0_20_41() {
    let fixture = UpgradeFixture::new("ck-upgrade-no-downgrade");
    fixture.set_core_version("0.20.83");
    let cache = fixture._root.path().join("update-metadata.json");
    fs::write(&cache, serde_json::to_vec(&json!({
        "format_version": 3,
        "checked_at_unix_secs": now_ms() / 1000,
        "targets": {
            "ck": {"version": "0.20.41", "sha256": "aa".repeat(32)},
            "ck-subc": {"version": "0.20.41", "sha256": "bb".repeat(32)},
            "ck-subc-mcp": {"version": "0.20.41", "sha256": "cc".repeat(32), "reports_release_version": false}
        }
    })).unwrap()).unwrap();
    let index = serve_signed_index(|base| core_release_index(base, "0.20.41"));
    let manifest = fixture
        .home
        .join(".local/share/cortexkit/installer-manifest.json");
    let before = fs::read(&manifest).unwrap();
    for args in [
        &["upgrade"][..],
        &["upgrade", "--check"][..],
        &["upgrade", "--dry-run"][..],
        &[][..],
    ] {
        let mut command = fixture.command(&index, args);
        command
            .env("CK_TEST_CK_VERSION", "0.20.83")
            .env("CK_TEST_SUBC_MCP_VERSION", "0.1.0");
        if args.is_empty() {
            command.env("CK_TEST_BARE_REFRESH_BUDGET_MS", "30000");
        }
        let output = command.output().unwrap();
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
        let stdout = text(&output.stdout);
        assert!(!stdout.contains("0.20.41"), "stdout:\n{stdout}");
        if args.is_empty() {
            assert!(stdout.contains("updates: none"), "stdout:\n{stdout}");
        } else if !args.contains(&"--dry-run") {
            assert!(
                stdout.starts_with("Everything is up to date"),
                "stdout:\n{stdout}"
            );
        }
        assert_eq!(
            fs::read(&manifest).unwrap(),
            before,
            "upgrade must not mutate inventory"
        );
    }
}

#[test]
fn upgrade_check_still_offers_a_strictly_newer_core_release() {
    let fixture = UpgradeFixture::new("ck-upgrade-newer-core");
    fixture.set_core_version("0.20.83");
    let index = serve_signed_index(|base| core_release_index(base, "0.20.84"));
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .env("CK_TEST_CK_VERSION", "0.20.83")
        .env("CK_TEST_SUBC_MCP_VERSION", "0.1.0")
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for update in [
        "ck 0.20.83 → 0.20.84",
        "ck-subc 0.20.83 → 0.20.84",
        "ck-subc-mcp → release 0.20.84",
    ] {
        assert!(stdout.contains(update), "stdout:\n{stdout}");
    }
}

#[test]
fn upgrade_check_names_a_version_exempt_release_without_a_double_space() {
    let fixture = UpgradeFixture::new("ck-upgrade-exempt-check");
    let index = serve_signed_index(|base| {
        let (mut index, assets) = upgrade_index(
            base,
            "0.55.1",
            &"22".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        );
        let asset = &mut index["components"]["core"]["assets"][host_target()]["ck-subc-mcp"];
        asset["sha256"] = json!("aa".repeat(32));
        asset["reports"] = Value::Null;
        (index, assets)
    });
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "ck-subc-mcp → release 0.17.9. Run ck upgrade.\n"
    );
}

#[test]
fn upgrade_check_reports_updates_when_the_daemon_is_down() {
    let fixture = UpgradeFixture::new("ck-upgrade-check-down");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .env("CK_TEST_DAEMON_UNREACHABLE", "daemon is down")
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "ck-aft 0.55.1 → 0.55.2. Run ck upgrade.\n"
    );
}

#[test]
fn upgrade_dry_run_exits_nonzero_when_activation_is_refused() {
    let fixture = UpgradeFixture::new("ck-upgrade-preview-down");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade", "--dry-run"])
        .env("CK_TEST_DAEMON_UNREACHABLE", "daemon is down")
        .output()
        .unwrap();
    assert_exit(&output, 1);
    assert!(text(&output.stdout).contains("daemon is unreachable: daemon is down"));
}

#[test]
fn upgrade_check_names_the_one_available_update_and_command() {
    let fixture = UpgradeFixture::new("ck-upgrade-check");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "ck-aft 0.55.1 → 0.55.2. Run ck upgrade.\n"
    );
}

#[test]
fn upgrade_apply_reports_completed_targets_then_done() {
    let fixture = UpgradeFixture::new("ck-upgrade-apply");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.10",
            &"bb".repeat(32),
            "0.8.1",
            &"cc".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade"])
        .env("CK_TEST_UPGRADE_APPLY_OK", "1")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "upgraded ck-aft 0.55.1 → 0.55.2, restarted\nupgraded ck-claustrum 0.8.0 → 0.8.1, restarted\nupgraded ck 0.17.9 → 0.17.10\nDone.\n"
    );
}

#[test]
fn upgrade_verbose_keeps_every_preexisting_outcome_line() {
    let fixture = UpgradeFixture::new("ck-upgrade-verbose");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.1",
            &"22".repeat(32),
            "0.17.9",
            &"44".repeat(32),
            "0.8.0",
            &"55".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade", "--verbose"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for outcome in [
        "  outcome: no-op: ck-subc-mcp is already current",
        "  outcome: no-op: ck-aft is already current",
        "  outcome: no-op: ck-claustrum is already current",
        "  outcome: no-op: ck-auth is already current",
        "  outcome: no-op: ck-subc is already current",
        "  outcome: no-op: ck is already current",
    ] {
        assert!(stdout.contains(outcome), "missing {outcome:?}:\n{stdout}");
    }
}

#[test]
fn daemon_lint_uses_its_explicit_config_without_a_daemon_connection() {
    let temp = TempDir::new("ck-daemon-lint");
    let config = temp.path().join("subc.jsonc");
    fs::write(&config, r#"{"version":1,"modules":{}}"#).unwrap();

    let output = ck_command()
        .args(["daemon", "lint"])
        .arg(&config)
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("checked 0 of 0 configured modules"),
        "stdout: {}",
        text(&output.stdout)
    );
}

// A copy of ck under a `ck-<name>` filename on PATH is what cargo test
// produces on Windows, where target/debug and its deps directory are
// prepended to PATH and every other build of ck sits there as `ck-<hash>`.
// The probe ck sends to domains is `--ck-domain`; if ck answered that flag
// by rendering help, help would discover domains, discover the copy, probe
// it, and the copy would do the same: each level waits out the two-second
// probe deadline and leaves orphans that keep spawning. Two properties keep
// that impossible, each proven here against real copies of ck rather than
// scripts, and measured by counting the probes a command launches (the
// test build logs each one as the parent spawns it), because a timing bound
// cannot tell a probe deadline from a loaded host: ck refuses `--ck-domain`
// before any discovery, so the probe launches nothing; and a ck copy is
// never listed as a domain by `--help`, which probes each copy exactly once
// and, since each copy refuses without probing, launches nothing further.
/// The one test allowed to place production-named (`ck-*`) executables.
const TWIN_TEST: &str = "a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed";

const PRODUCTION_NAME_EXEMPTIONS: &[(&str, &str)] =
    &[(TWIN_TEST, "ck-twin"), (TWIN_TEST, "ck-twin-two")];

/// The control for the twin exemption: in this same file, a production-named
/// spawn outside it is still refused, whether it borrows the twin test's name,
/// names a file the exemption does not list, or skips the exemption entirely.
#[test]
fn a_production_named_spawn_outside_the_twin_exemption_is_refused() {
    let bin = Path::new("/fixture/bin");
    let refused = |attempt: &dyn Fn()| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(attempt)).is_err()
    };
    assert!(
        refused(&|| {
            cortexkit_test_support::exempt_production_executable(
                TWIN_TEST,
                &bin.join("ck-twin"),
                PRODUCTION_NAME_EXEMPTIONS,
            );
        }),
        "another test must not borrow the twin test's exemption"
    );
    assert!(
        refused(&|| {
            cortexkit_test_support::exempt_production_executable(
                "a_production_named_spawn_outside_the_twin_exemption_is_refused",
                &bin.join("ck-twin"),
                PRODUCTION_NAME_EXEMPTIONS,
            );
        }),
        "the exemption names one test"
    );
    assert!(
        refused(&|| {
            let _ = cortexkit_test_support::dev_command(bin.join(platform_binary("ck-twin")));
        }),
        "the spawn guard refuses a ck-* program"
    );
}

#[test]
fn a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed() {
    let temp = TempDir::new("ck-twin-on-path");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("create fake PATH");
    let invocations = temp.path().join("probes.log");
    let count_probes = || {
        fs::read_to_string(&invocations)
            .map(|log| {
                log.lines()
                    .filter(|line| line.starts_with("probe "))
                    .count()
            })
            .unwrap_or(0)
    };
    // Two copies, because a ck never probes its own executable: the CI shape
    // has many builds of ck side by side, and the recursion runs between
    // them, each copy probing the other.
    //
    // These copies keep `ck-` names on purpose, the one exemption from the
    // rule that test processes run as `ckdev-*`: what this test checks is
    // ck's production discovery of external domains, which matches `ck-*`
    // executables on PATH by design, so a `ckdev-` copy would never be probed
    // and the test would check something else. ck itself, not this test,
    // starts them, and each refuses `--ck-domain` and exits within
    // milliseconds. The exemption names this test and these two file names
    // exactly; any other production-named placement is refused.
    for name in ["ck-twin", "ck-twin-two"] {
        let copy = bin.join(platform_binary(name));
        common::copy_executable(
            Path::new(env!("CARGO_BIN_EXE_ck-under-test")),
            cortexkit_test_support::exempt_production_executable(
                TWIN_TEST,
                &copy,
                PRODUCTION_NAME_EXEMPTIONS,
            ),
        );
    }
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&system_path_only())),
    )
    .expect("fixture PATH");

    // The probe itself: ck must answer without discovering anything, so it
    // launches no probe of its own.
    let probed = ck_command()
        .arg("--ck-domain")
        .env("PATH", &path)
        .env("CK_TEST_INVOCATION_LOG", &invocations)
        .output()
        .expect("probe ck itself");
    assert_eq!(
        count_probes(),
        0,
        "ck --ck-domain launched probes:\n{}",
        fs::read_to_string(&invocations).unwrap_or_default()
    );
    assert_exit(&probed, 2);
    assert!(
        probed.stdout.is_empty(),
        "a probed ck must print no headline:\n{}",
        text(&probed.stdout)
    );
    assert!(
        text(&probed.stderr).contains("not a domain"),
        "refusal names the reason:\n{}",
        text(&probed.stderr)
    );

    // Help discovers: each copy is probed exactly once, the copies launch
    // no probes of their own (they inherit the log and would show here),
    // and neither is listed.
    fs::remove_file(&invocations).ok();
    let help = ck_command()
        .arg("--help")
        .env("PATH", &path)
        .env("CK_TEST_INVOCATION_LOG", &invocations)
        .output()
        .expect("run help with a ck copy on PATH");
    assert_exit(&help, 0);
    assert_eq!(
        count_probes(),
        2,
        "help must probe each of the two copies exactly once and nothing else:\n{}",
        fs::read_to_string(&invocations).unwrap_or_default()
    );
    let help_text = text(&help.stdout);
    assert!(
        !help_text.contains("\n  twin"),
        "a copy of ck was listed as a domain:\n{help_text}"
    );

    // An unknown flag takes the same no-discovery path: one line and the
    // next step, never the rendered command tree.
    let unknown = ck_command()
        .arg("--no-such-flag")
        .env("PATH", &path)
        .output()
        .expect("unknown flag");
    assert_exit(&unknown, 2);
    assert_eq!(
        text(&unknown.stderr).trim(),
        "unknown flag '--no-such-flag'. Run ck --help."
    );
}

/// One program can carry several faces chosen by argv[0], installed as
/// symlinks (entorhinal's `ck-projects` and `ck-workspaces` both link to
/// `ck-entorhinal`). ck must probe and run the name it found on PATH, not the
/// file the link points to, or every face arrives as the module and the
/// domain can never answer its handshake.
#[cfg(unix)]
#[test]
fn a_domain_installed_as_a_symlink_is_probed_and_run_by_its_own_name() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let temp = TempDir::new("ck-domain-symlink-face");
    let bin = temp.path().join("bin");
    let store = temp.path().join("store");
    fs::create_dir_all(&bin).expect("create fake PATH");
    fs::create_dir_all(&store).expect("create program dir");
    // Answers as a domain only under the face name; under its own file name
    // it refuses the probe exactly as a module binary does. `${0##*/}` rather
    // than basename: PATH holds only the fake bin, so no external command runs.
    let program = store.join("multi-face");
    fs::write(
        &program,
        "#!/bin/sh\ncase \"${0##*/}\" in\n  ck-faced)\n    if [ \"$1\" = \"--ck-domain\" ]; then echo \"faced domain headline\"; exit 0; fi\n    echo \"ran as ck-faced: $*\"; exit 0 ;;\n  *) echo \"unknown flag\" >&2; exit 64 ;;\nesac\n",
    )
    .expect("write multi-face program");
    let mut permissions = fs::metadata(&program).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&program, permissions).expect("mark executable");
    symlink(&program, bin.join("ck-faced")).expect("link the face");

    let cache = temp.path().join("update-metadata.json");
    let dispatched = ck_command()
        .args(["faced", "list"])
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("dispatch the symlinked face");
    assert_exit(&dispatched, 0);
    assert_eq!(text(&dispatched.stdout).trim(), "ran as ck-faced: list");
}

#[cfg(unix)]
fn write_domain_program(directory: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(directory).expect("create fake domain directory");
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake domain");
    let mut permissions = fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("mark fake domain executable");
    path
}

#[cfg(unix)]
#[test]
fn domain_in_cortexkit_bin_is_discovered_and_dispatched() {
    let temp = TempDir::new("ck-domain-data-bin");
    let data_home = temp.path().join("data-home");
    let domain_bin = data_home.join("cortexkit").join("bin");
    write_domain_program(
        &domain_bin,
        "ck-local-only",
        "if [ \"$1\" = \"--ck-domain\" ]; then echo 'local domain'; exit 0; fi\necho dispatched-from-cortexkit-bin",
    );

    let dispatched = ck_command()
        .args(["local-only", "list"])
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", system_path_only())
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("dispatch a domain installed only in CortexKit bin");
    assert_exit(&dispatched, 0);
    assert_eq!(
        text(&dispatched.stdout).trim(),
        "dispatched-from-cortexkit-bin"
    );
}

#[cfg(unix)]
#[test]
fn cortexkit_bin_domain_takes_precedence_over_path_domain() {
    let temp = TempDir::new("ck-domain-bin-precedence");
    let data_home = temp.path().join("data-home");
    let domain_bin = data_home.join("cortexkit").join("bin");
    let path_bin = temp.path().join("path-bin");
    write_domain_program(
        &domain_bin,
        "ck-priority",
        "if [ \"$1\" = \"--ck-domain\" ]; then echo 'bin-folder headline'; exit 0; fi\necho dispatched-from-cortexkit-bin",
    );
    write_domain_program(
        &path_bin,
        "ck-priority",
        "if [ \"$1\" = \"--ck-domain\" ]; then echo 'PATH headline'; exit 0; fi\necho dispatched-from-PATH",
    );

    let dispatched = ck_command()
        .args(["priority", "list"])
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", &path_bin)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("dispatch the higher-priority domain");
    assert_exit(&dispatched, 0);
    assert_eq!(
        text(&dispatched.stdout).trim(),
        "dispatched-from-cortexkit-bin"
    );

    let help = ck_command()
        .arg("--help")
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", &path_bin)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("list the selected domain once");
    assert_exit(&help, 0);
    let help_text = text(&help.stdout);
    assert!(help_text.contains("bin-folder headline"), "{help_text}");
    assert!(!help_text.contains("PATH headline"), "{help_text}");
    assert_eq!(
        help_text
            .lines()
            .filter(|line| line.contains("priority"))
            .count(),
        1,
        "a domain present in both locations must be listed only once:\n{help_text}"
    );
}

#[cfg(unix)]
#[test]
fn non_domain_in_cortexkit_bin_is_neither_listed_nor_dispatched() {
    let temp = TempDir::new("ck-domain-bin-nondomain");
    let data_home = temp.path().join("data-home");
    let domain_bin = data_home.join("cortexkit").join("bin");
    write_domain_program(
        &domain_bin,
        "ck-not-domain",
        "if [ \"$1\" = \"--ck-domain\" ]; then exit 1; fi\necho SHOULD_NOT_DISPATCH",
    );

    let help = ck_command()
        .arg("--help")
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", system_path_only())
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("list domains without the non-domain binary");
    assert_exit(&help, 0);
    assert!(
        !text(&help.stdout).contains("not-domain"),
        "a binary that declined the handshake was listed:\n{}",
        text(&help.stdout)
    );

    let refused = ck_command()
        .arg("not-domain")
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", system_path_only())
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("refuse to dispatch the non-domain binary");
    assert_exit(&refused, 1);
    assert!(
        refused.stdout.is_empty(),
        "non-domain binary was dispatched"
    );
    assert!(
        text(&refused.stderr).contains("is not a ck command"),
        "the refusal should explain why dispatch was denied:\n{}",
        text(&refused.stderr)
    );
}

#[cfg(unix)]
#[test]
fn path_only_domain_is_discovered_and_dispatched() {
    let temp = TempDir::new("ck-domain-path-only");
    let data_home = temp.path().join("data-home");
    let path_bin = temp.path().join("path-bin");
    fs::create_dir_all(&data_home).expect("create isolated data home without a domain");
    write_domain_program(
        &path_bin,
        "ck-path-only",
        "if [ \"$1\" = \"--ck-domain\" ]; then echo 'PATH-only domain'; exit 0; fi\necho dispatched-from-PATH",
    );

    let dispatched = ck_command()
        .args(["path-only", "list"])
        .env("XDG_DATA_HOME", &data_home)
        .env("PATH", &path_bin)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", "8000")
        .output()
        .expect("dispatch a domain found only on PATH");
    assert_exit(&dispatched, 0);
    assert_eq!(text(&dispatched.stdout).trim(), "dispatched-from-PATH");
}

#[cfg(unix)]
#[test]
fn mcp_gateway_domain_is_listed_and_invoked_through_ck() {
    let temp = TempDir::new("ck-mcp-domain");
    let data_home = temp.path().join("data-home");
    let domain_bin = data_home.join("cortexkit").join("bin");
    write_domain_program(
        &domain_bin,
        "ck-subc-mcp",
        "if [ \"$1\" = \"--ck-domain\" ]; then printf '%s\\n' 'CortexKit MCP gateway'; exit 0; fi\nprintf 'forwarded:'\nfor arg in \"$@\"; do printf ' <%s>' \"$arg\"; done\nprintf '\\n'",
    );

    let help = ck_command()
        .arg("--help")
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .expect("render command help with the MCP gateway");
    assert_exit(&help, 0);
    assert!(
        text(&help.stdout).contains("subc-mcp  CortexKit MCP gateway"),
        "the opted-in MCP gateway should be listed:\n{}",
        text(&help.stdout)
    );

    let dispatched = ck_command()
        .args(["subc-mcp", "shim", "--harness", "opencode"])
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .expect("dispatch the MCP shim through ck");
    assert_exit(&dispatched, 0);
    assert_eq!(
        text(&dispatched.stdout),
        "forwarded: <shim> <--harness> <opencode>\n"
    );
}

#[cfg(unix)]
#[test]
fn external_domain_replaces_ck_and_preserves_signal_behavior() {
    use std::os::unix::process::ExitStatusExt;

    fn wait_for_path(path: &Path, duration: Duration) -> bool {
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        path.exists()
    }

    fn wait_for_child(
        child: &mut std::process::Child,
        duration: Duration,
    ) -> Option<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + duration;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    fn signal_pid(pid: u32, signal: &str) {
        let status = Command::new("kill")
            .args([format!("-{signal}"), pid.to_string()])
            .status()
            .expect("send a signal to the domain process");
        assert!(status.success(), "kill -{signal} {pid} failed: {status}");
    }

    fn pid_is_running(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_for_pid_exit(pid: u32, duration: Duration) -> bool {
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            if !pid_is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !pid_is_running(pid)
    }

    let temp = TempDir::new("ck-domain-signals");
    let data_home = temp.path().join("data-home");
    let domain_bin = data_home.join("cortexkit").join("bin");
    write_domain_program(
        &domain_bin,
        "ck-signal-probe",
        "if [ \"$1\" = \"--ck-domain\" ]; then printf '%s\\n' 'Signal fixture'; exit 0; fi\n\
         trap 'printf \"term\\n\" > \"$CK_DOMAIN_TERM_MARKER\"; exit 7' TERM\n\
         printf '%s\\n' \"$$\" > \"$CK_DOMAIN_STARTED_MARKER\"\n\
         while :; do sleep 0.05; done",
    );

    let started = temp.path().join("term-started");
    let term_marker = temp.path().join("term-received");
    let mut ck = ck_command()
        .arg("signal-probe")
        .env("XDG_DATA_HOME", &data_home)
        .env("CK_DOMAIN_STARTED_MARKER", &started)
        .env("CK_DOMAIN_TERM_MARKER", &term_marker)
        .spawn()
        .expect("start ck with the signal fixture");
    let started_in_time = wait_for_path(&started, Duration::from_secs(5));
    if !started_in_time {
        let _ = ck.kill();
        let _ = ck.wait();
    }
    assert!(started_in_time, "domain fixture did not start");
    let domain_pid: u32 = fs::read_to_string(&started)
        .expect("read domain process pid")
        .trim()
        .parse()
        .expect("domain process pid is numeric");
    signal_pid(ck.id(), "TERM");
    let term_status = wait_for_child(&mut ck, Duration::from_secs(5));
    let term_marker_written = wait_for_path(&term_marker, Duration::from_secs(3));
    if !term_marker_written {
        // Send SIGTERM directly so a failed test never leaves the fixture running.
        signal_pid(domain_pid, "TERM");
        let _ = wait_for_path(&term_marker, Duration::from_secs(3));
    }
    let domain_stopped = wait_for_pid_exit(domain_pid, Duration::from_secs(3));
    let term_status = term_status.unwrap_or_else(|| {
        let _ = ck.kill();
        ck.wait().expect("reap ck after the bounded wait")
    });
    assert!(
        domain_stopped,
        "domain script remained running after SIGTERM"
    );
    assert!(
        term_marker_written,
        "the domain process did not handle SIGTERM"
    );
    assert_eq!(term_status.code(), Some(7));

    let started = temp.path().join("sigkill-started");
    let mut ck = ck_command()
        .arg("signal-probe")
        .env("XDG_DATA_HOME", &data_home)
        .env("CK_DOMAIN_STARTED_MARKER", &started)
        .env(
            "CK_DOMAIN_TERM_MARKER",
            temp.path().join("sigkill-term-received"),
        )
        .spawn()
        .expect("start ck for the SIGKILL fixture");
    let started_in_time = wait_for_path(&started, Duration::from_secs(5));
    if !started_in_time {
        let _ = ck.kill();
        let _ = ck.wait();
    }
    assert!(started_in_time, "domain fixture did not start for SIGKILL");
    let domain_pid: u32 = fs::read_to_string(&started)
        .expect("read domain process pid")
        .trim()
        .parse()
        .expect("domain process pid is numeric");
    signal_pid(domain_pid, "KILL");
    let status = wait_for_child(&mut ck, Duration::from_secs(5)).unwrap_or_else(|| {
        let _ = ck.kill();
        ck.wait().expect("reap ck after the bounded SIGKILL wait")
    });
    assert_eq!(status.signal(), Some(9));
    assert!(
        wait_for_pid_exit(domain_pid, Duration::from_secs(3)),
        "domain script remained running after SIGKILL"
    );
}

#[cfg(unix)]
#[test]
fn external_domains_opt_in_dispatch_and_cache_their_probe() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new("ck-domain-probes");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("create fake PATH");
    let count = temp.path().join("yes-probe-count");
    let cache = temp.path().join("update-metadata.json");
    let write_program = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake domain");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("mark fake domain executable");
    };
    write_program(
        "ck-yes",
        "if [ \"$1\" = \"--ck-domain\" ]; then printf x >> \"$CK_DOMAIN_PROBE_COUNT\"; echo \"helpful fake domain\"; exit 0; fi\necho dispatched-yes",
    );
    write_program("ck-no", "exit 1");
    // Far longer than the probe deadline, so the elapsed-time bound below
    // distinguishes "the deadline fired" from "the probe finished on its own".
    write_program("ck-hang", "sleep 30");
    write_program("ck-aft", "exit 1");
    write_program("ck-mc", "exit 1");
    // A rollback snapshot beside a live binary. It is the SAME program as
    // ck-yes and would answer the handshake if asked; the count below proves
    // it is never asked, because a dotted name is not a domain.
    write_program(
        "ck-yes.bak-20260914T004527",
        "if [ \"$1\" = \"--ck-domain\" ]; then printf x >> \"$CK_DOMAIN_PROBE_COUNT\"; echo \"stale snapshot answering\"; exit 0; fi",
    );

    // The production probe deadline is two seconds; under a full parallel
    // suite a shell-script domain has taken longer than that just to start,
    // and a genuine domain then read as refused. The test build accepts a
    // wider deadline, which changes nothing about what is proven: the hang
    // is 30 s, so returning well under that still shows the deadline fired.
    let probe_deadline_ms = "8000";
    let started = std::time::Instant::now();
    let help = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", probe_deadline_ms)
        .output()
        .expect("run help");
    assert_exit(&help, 0);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a hanging probe must stop at the probe deadline, not run to completion (took {:?})",
        started.elapsed()
    );
    let help_text = text(&help.stdout);
    assert!(
        help_text.contains("yes       helpful fake domain"),
        "approved domain is listed with its headline:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  no "),
        "refusing binary leaked into help:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  hang "),
        "hanging binary leaked into help:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  aft "),
        "module binary leaked into help:\n{help_text}"
    );
    assert_eq!(
        fs::read_to_string(&count).expect("probe count"),
        "x",
        "exactly one probe: the dotted rollback snapshot must never be spawned"
    );
    assert!(
        !help_text.contains("yes.bak"),
        "rollback snapshot leaked into help as a domain:\n{help_text}"
    );

    let second_help = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", probe_deadline_ms)
        .output()
        .expect("run cached help");
    assert_exit(&second_help, 0);
    assert_eq!(
        fs::read_to_string(&count).expect("cached probe count"),
        "x",
        "a matching executable stamp must skip the probe"
    );

    let refusing = ck_command()
        .arg("no")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject unapproved domain");
    assert_exit(&refusing, 1);
    assert!(
        refusing.stdout.is_empty(),
        "unapproved domain was dispatched"
    );

    let dispatched = ck_command()
        .arg("yes")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", probe_deadline_ms)
        .output()
        .expect("dispatch approved domain");
    assert_exit(&dispatched, 0);
    assert_eq!(text(&dispatched.stdout).trim(), "dispatched-yes");

    fs::write(
        bin.join("ck-yes"),
        "#!/bin/sh\nif [ \"$1\" = \"--ck-domain\" ]; then printf x >> \"$CK_DOMAIN_PROBE_COUNT\"; echo \"helpful fake domain\"; exit 0; fi\necho dispatched-yes\n# changed\n",
    )
    .expect("change fake domain");
    let refreshed = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .env("CK_TEST_DOMAIN_PROBE_TIMEOUT_MS", probe_deadline_ms)
        .output()
        .expect("run changed help");
    assert_exit(&refreshed, 0);
    assert_eq!(
        fs::read_to_string(&count).expect("changed probe count"),
        "xx",
        "a changed binary must be probed again"
    );

    let module = ck_command()
        .arg("aft")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject module binary");
    assert_exit(&module, 1);
    assert_eq!(
        text(&module.stderr).trim(),
        "'aft' is a module, not a command. Try: ck module status aft"
    );

    let mc = ck_command()
        .arg("mc")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject magic-context module binary");
    assert_exit(&mc, 1);
    assert_eq!(
        text(&mc.stderr).trim(),
        "'mc' is a module, not a command. Try: ck module status magic-context"
    );

    let unknown = ck_command()
        .arg("nosuch")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject unknown command");
    assert_exit(&unknown, 1);
    assert!(
        text(&unknown.stderr)
            .contains("unknown command 'nosuch'. No ck-nosuch in the CortexKit bin folder ("),
        "unknown-command diagnostic did not name the managed bin folder:\n{}",
        text(&unknown.stderr)
    );
    assert!(
        text(&unknown.stderr).contains("from XDG_DATA_HOME) or on PATH. Run ck --help."),
        "unknown-command diagnostic did not name its search sources:\n{}",
        text(&unknown.stderr)
    );
    assert!(!text(&unknown.stderr).contains("usage:"));
}

#[cfg(not(windows))]
#[test]
fn unknown_command_names_managed_bin_folder_from_xdg_data_home() {
    let temp = TempDir::new("ck-unknown-xdg-data-home");
    let home = temp.path().join("home");
    let xdg_data_home = home.join("xdg");
    let path = temp.path().join("path");
    fs::create_dir_all(&home).expect("create isolated home");
    fs::create_dir_all(&path).expect("create isolated PATH");

    let output = ck_command()
        .arg("nosuchdomain")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("PATH", &path)
        .output()
        .expect("run unknown command with XDG data home");
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr).trim(),
        "unknown command 'nosuchdomain'. No ck-nosuchdomain in the CortexKit bin folder (~/xdg/cortexkit/bin, from XDG_DATA_HOME) or on PATH. Run ck --help."
    );
}

#[cfg(not(windows))]
#[test]
fn unknown_command_names_managed_bin_folder_from_home() {
    let temp = TempDir::new("ck-unknown-home-data-home");
    let home = temp.path().join("home");
    let path = temp.path().join("path");
    fs::create_dir_all(&home).expect("create isolated home");
    fs::create_dir_all(&path).expect("create isolated PATH");

    let output = ck_command()
        .arg("nosuchdomain")
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .env("PATH", &path)
        .output()
        .expect("run unknown command with HOME data home");
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr).trim(),
        "unknown command 'nosuchdomain'. No ck-nosuchdomain in the CortexKit bin folder (~/.local/share/cortexkit/bin, from HOME) or on PATH. Run ck --help."
    );
}

#[test]
fn mc_hint_names_the_daemon_module_id() {
    let output = ck_command().arg("mc").output().unwrap();
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr),
        "'mc' is a module, not a command. Try: ck module status magic-context\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_logs_merges_sources_by_timestamp_and_reports_real_filter_counts() {
    let module_id = "merged-logs";
    let data_home = unique_temp_dir("ck-module-logs-data");
    let module_logs = data_home.join("cortexkit").join(module_id).join("logs");
    let run_logs = data_home.join("cortexkit").join("run").join("logs");
    fs::create_dir_all(&module_logs).unwrap();
    fs::create_dir_all(&run_logs).unwrap();
    // Fleet-logging r2 layout: ONE dated segment per module carries every lane
    // (the module process and each harness plugin), told apart by the
    // `harness=` bound field on the line rather than by file name. The
    // daemon's own log is `subc.<date>.log` in the run directory. The stderr
    // capture is raw child bytes and is deliberately not in the fleet format.
    fs::write(
        module_logs.join(format!("{module_id}.2026-09-05.log")),
        format!(
            "2026-09-05T10:00:00.000Z DEBUG {module_id}: hidden debug\n\
             2026-09-05T10:00:01.000Z INFO  {module_id}: [harness=opencode] plugin line\n\
             2026-09-05T10:00:03.000Z INFO  {module_id}.perf: module line ms=7\n"
        ),
    )
    .unwrap();
    fs::write(
        run_logs.join(format!("{module_id}.stderr.log")),
        // Genuinely raw child output: a Rust panic line carries no timestamp
        // or level. (An r1-shaped line here would now parse, correctly, since
        // pre-adoption modules wrote r1 to stderr.)
        "thread 'main' panicked at src/main.rs:12:5: captured line\n",
    )
    .unwrap();
    fs::write(
        run_logs.join("subc.2026-09-05.log"),
        format!(
            "2026-09-05T10:00:02.000Z WARN  subc: daemon line module_id={module_id}\n\
             2026-09-05T10:00:05.000Z INFO  subc: unrelated module_id=other\n"
        ),
    )
    .unwrap();
    let daemon = ScriptedDaemon::start(
        "ck-module-logs-merge",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::Reply(Box::new(ClientControlResponse::SupervisorList {
                generation: 1,
                modules: vec![scripted_supervisor_entry(module_id, None)],
            })),
        }],
    )
    .await;

    let output = ck_command()
        .args([
            "module", "logs", module_id, "-n", "20", "--level", "info", "--subc",
        ])
        .arg(&daemon.connection_file_path)
        .env("XDG_DATA_HOME", &*data_home)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        concat!(
            "mod          2026-09-05T10:00:01.000Z INFO  merged-logs: [harness=opencode] plugin line\n",
            "daemon       2026-09-05T10:00:02.000Z WARN  subc: daemon line module_id=merged-logs\n",
            "mod          2026-09-05T10:00:03.000Z INFO  merged-logs.perf: module line ms=7\n",
            "stderr       thread 'main' panicked at src/main.rs:12:5: captured line\n"
        )
    );
    assert_eq!(
        text(&output.stderr),
        // The stderr capture line is raw child bytes, not fleet format, and the
        // summary says so rather than counting it as if it had parsed.
        "showing 4 of 6 lines (1 below requested level, 1 daemon lines hidden, 1 unparsed lines shown)\n"
    );
}

/// A capture file begun before daemon stamping has an unstamped head.
/// Those lines inherit the first following stamp, so they stay at the
/// beginning of the capture lane instead of becoming the merged tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_logs_orders_legacy_capture_head_before_stamped_tail() {
    let module_id = "stamped-capture";
    let data_home = unique_temp_dir("ck-module-logs-stamped-data");
    let runtime_dir = unique_temp_dir("ck-module-logs-stamped-runtime");
    let config_home = unique_temp_dir("ck-module-logs-stamped-config");
    let module_logs = data_home.join("cortexkit").join(module_id).join("logs");
    let run_logs = data_home.join("cortexkit").join("run").join("logs");
    fs::create_dir_all(&module_logs).unwrap();
    fs::create_dir_all(&run_logs).unwrap();
    fs::write(
        module_logs.join(format!("{module_id}.2026-09-19.log")),
        format!("2026-09-19T07:04:03.000Z INFO  {module_id}: module line\n"),
    )
    .unwrap();
    fs::write(
        run_logs.join(format!("{module_id}.stderr.log")),
        concat!(
            // Written by a daemon that did not stamp capture lines yet.
            "old unstamped line\n",
            "2026-09-19T07:04:02.000Z first stamped line\n",
            "2026-09-19T07:04:05.000Z second stamped line\n",
        ),
    )
    .unwrap();
    fs::write(
        run_logs.join("subc.2026-09-19.log"),
        format!(
            "2026-09-19T07:04:01.000Z INFO  subc: spawned module_id={module_id}\n\
             2026-09-19T07:04:04.000Z WARN  subc: slow module_id={module_id}\n"
        ),
    )
    .unwrap();
    let daemon = ScriptedDaemon::start(
        "ck-module-logs-stamped",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::Reply(Box::new(ClientControlResponse::SupervisorList {
                generation: 1,
                modules: vec![scripted_supervisor_entry(module_id, None)],
            })),
        }],
    )
    .await;

    let output = ck_command()
        .args(["module", "logs", module_id, "-n", "20", "--subc"])
        .arg(&daemon.connection_file_path)
        .env("XDG_DATA_HOME", &*data_home)
        .env("XDG_RUNTIME_DIR", &*runtime_dir)
        .env("XDG_CONFIG_HOME", &*config_home)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        concat!(
            "daemon       2026-09-19T07:04:01.000Z INFO  subc: spawned module_id=stamped-capture\n",
            "stderr       old unstamped line\n",
            "stderr       2026-09-19T07:04:02.000Z first stamped line\n",
            "mod          2026-09-19T07:04:03.000Z INFO  stamped-capture: module line\n",
            "daemon       2026-09-19T07:04:04.000Z WARN  subc: slow module_id=stamped-capture\n",
            "stderr       2026-09-19T07:04:05.000Z second stamped line\n",
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_logs_empty_states_are_verbatim() {
    let data_home = unique_temp_dir("ck-module-logs-empty");
    let known = "quiet-module";
    let known_daemon = ScriptedDaemon::start(
        "ck-module-logs-known-empty",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::Reply(Box::new(ClientControlResponse::SupervisorList {
                generation: 1,
                modules: vec![scripted_supervisor_entry(known, None)],
            })),
        }],
    )
    .await;
    let known_output = ck_command()
        .args(["module", "logs", known, "--subc"])
        .arg(&known_daemon.connection_file_path)
        .env("XDG_DATA_HOME", &*data_home)
        .output()
        .unwrap();
    assert_exit(&known_output, 0);
    assert_eq!(text(&known_output.stdout), "no log yet for quiet-module\n");
    assert!(known_output.stderr.is_empty());

    let unknown_daemon = ScriptedDaemon::start(
        "ck-module-logs-unknown",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::Reply(Box::new(ClientControlResponse::SupervisorList {
                generation: 1,
                modules: Vec::new(),
            })),
        }],
    )
    .await;
    let unknown_output = ck_command()
        .args(["module", "logs", "missing", "--subc"])
        .arg(&unknown_daemon.connection_file_path)
        .env("XDG_DATA_HOME", &*data_home)
        .output()
        .unwrap();
    assert_exit(&unknown_output, 0);
    assert_eq!(
        text(&unknown_output.stdout),
        "no module named 'missing'. Run ck module list.\n"
    );
    assert!(unknown_output.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_json_uses_subc_override_and_shows_stub() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-list-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let hidden_runtime = TempDir::new("ck-hidden-runtime");
    let hidden_home = TempDir::new("ck-hidden-home");
    let hidden_tmp = TempDir::new("ck-hidden-tmp");

    let output = ck_command()
        .args(["module", "list", "--json", "--subc"])
        .arg(&server.connection_file_path)
        .env("XDG_RUNTIME_DIR", hidden_runtime.path())
        .env("HOME", hidden_home.path())
        .env("TMPDIR", hidden_tmp.path())
        .env("TMP", hidden_tmp.path())
        .env("TEMP", hidden_tmp.path())
        .output()
        .unwrap();
    let json_stdout = text(&output.stdout);
    let value = assert_json_success(output);
    let modules = value["modules"].as_array().unwrap();
    assert!(
        modules
            .iter()
            .any(|module| module["module_id"] == module_id),
        "ck module list --json should include the supervised stub: {value}"
    );
    let entry = modules
        .iter()
        .find(|module| module["module_id"] == module_id)
        .unwrap();
    assert_eq!(entry["pending_reload"]["spec"]["status"], "match");
    if cfg!(windows) {
        assert_eq!(entry["launch_nonce_source"], "pipe");
    } else {
        assert!(entry.get("launch_nonce_source").is_none());
    }

    let text_output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&text_output, 0);
    let text_stdout = text(&text_output.stdout);
    let expected_list = if cfg!(windows) {
        "module        status   health   reload   nonce source\nck-list-stub  running  unknown  nothing  pipe        \n"
    } else if cfg!(any(target_os = "linux", target_os = "macos")) {
        "module        status   health   reload \nck-list-stub  running  unknown  nothing\n"
    } else {
        "module        status   health   reload             \nck-list-stub  running  unknown  nothing (image n/a)\n"
    };
    assert_eq!(text_stdout, expected_list);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer: {json_stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_renders_status_words_not_wire_booleans() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&output, 0);
    let expected_list = if cfg!(windows) {
        "module  status   health    reload   nonce source\ninsula  running  degraded  nothing  pipe        \n"
    } else if cfg!(any(target_os = "linux", target_os = "macos")) {
        "module  status   health    reload \ninsula  running  degraded  nothing\n"
    } else {
        "module  status   health    reload             \ninsula  running  degraded  nothing (image n/a)\n"
    };
    assert_eq!(text(&output.stdout), expected_list);
    assert!(!text(&output.stdout).contains("true"));
    assert!(!text(&output.stdout).contains("false"));

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_keeps_configured_path_mismatch_pending_on_every_platform() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "path-mismatch";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let replacement_home = TempDir::new("ck-list-replacement");
    let replacement = replacement_home.path().join(
        Path::new(env!("CARGO_BIN_EXE_fake-aft-stub"))
            .file_name()
            .unwrap(),
    );
    fs::copy(env!("CARGO_BIN_EXE_fake-aft-stub"), &replacement).unwrap();
    let mut configured = stub_spec(module_id);
    configured.program = replacement.clone();
    module.update_spec_for_test(configured).await.unwrap();

    let json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "list", "--json"],
    ));
    let entry = &json["modules"][0];
    assert_eq!(entry["module_id"], module_id);
    assert_eq!(entry["pending_reload"]["path"]["status"], "mismatch");
    assert_eq!(
        entry["pending_reload"]["path"]["configured"],
        json!(replacement)
    );
    if cfg!(windows) {
        // Configuration points elsewhere, but the retained child still runs
        // the unchanged file at its original spawn path. Image agreement must
        // not erase the independent configured-path mismatch above.
        assert_eq!(entry["pending_reload"]["image"]["status"], "match");
        assert_eq!(entry["launch_nonce_source"], "pipe");
    } else if !cfg!(any(target_os = "linux", target_os = "macos")) {
        assert_eq!(entry["pending_reload"]["image"]["status"], "unavailable");
        assert_eq!(
            entry["pending_reload"]["image"]["reason"],
            "unsupported_platform"
        );
    }

    let output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let expected_suffix = if cfg!(windows) {
        "pending (path+spec)  pipe"
    } else {
        "pending (path+spec)"
    };
    assert!(
        stdout.lines().any(|line| {
            line.starts_with(module_id) && line.trim_end().ends_with(expected_suffix)
        }),
        "{stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_inputs_change_pending_spec_until_restart() {
    for dimension in ["env", "args", "protocol"] {
        let server = TestServer::start().await;
        let supervisor = supervisor(&server);
        let module_id = "spec-change";
        let mut spec = stub_spec(module_id);
        if dimension != "env" {
            spec.env.push(("FAKE_AFT_NEVER_CONNECT".into(), "1".into()));
        }
        if dimension == "args" {
            spec.protocol = ModuleProtocol::None;
        }
        let module = supervisor.spawn(spec.clone()).unwrap();
        let before = assert_json_success(ck_with_subc(
            &server.connection_file_path,
            ["module", "list", "--json"],
        ));
        assert_eq!(
            before["modules"][0]["pending_reload"]["spec"]["status"], "match",
            "{dimension}"
        );
        match dimension {
            "env" => spec
                .env
                .push(("FAKE_AFT_TOOLCALL_RESULT".into(), "updated".into())),
            "args" => spec.args.push("--different-launch-input".into()),
            "protocol" => spec.protocol = ModuleProtocol::None,
            _ => unreachable!(),
        }
        module.update_spec_for_test(spec).await.unwrap();
        let pending = assert_json_success(ck_with_subc(
            &server.connection_file_path,
            ["module", "list", "--json"],
        ));
        assert_eq!(
            pending["modules"][0]["pending_reload"]["spec"]["status"], "mismatch",
            "{dimension}"
        );
        let before_generation = module.status().unwrap().spawn_generation;
        module.restart(None).await.unwrap();
        wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
            entry.state == "running"
                && entry
                    .spawn_generation
                    .is_some_and(|generation| generation > before_generation)
        })
        .await;
        let after = assert_json_success(ck_with_subc(
            &server.connection_file_path,
            ["module", "list", "--json"],
        ));
        assert_eq!(
            after["modules"][0]["pending_reload"]["spec"]["status"], "match",
            "{dimension}"
        );
        module.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_connection_files_and_fresh_nonces_do_not_pending_reload() {
    let first = TestServer::start().await;
    let second = TestServer::start().await;
    assert_ne!(first.connection_file_path, second.connection_file_path);
    let first_supervisor = supervisor(&first);
    let second_supervisor = supervisor(&second);
    let first_module = spawn_stub(&first, &first_supervisor, "same-spec").await;
    let second_module = spawn_stub(&second, &second_supervisor, "same-spec").await;
    for server in [&first, &second] {
        let list = assert_json_success(ck_with_subc(
            &server.connection_file_path,
            ["module", "list", "--json"],
        ));
        assert_eq!(
            list["modules"][0]["pending_reload"]["spec"]["status"],
            "match"
        );
    }
    let before_generation = first_module.status().unwrap().spawn_generation;
    first_module.restart(None).await.unwrap();
    wait_for_supervisor_entry(&first.connection_file_path, "same-spec", |entry| {
        entry.state == "running"
            && entry
                .spawn_generation
                .is_some_and(|generation| generation > before_generation)
    })
    .await;
    let list = assert_json_success(ck_with_subc(
        &first.connection_file_path,
        ["module", "list", "--json"],
    ));
    assert_eq!(
        list["modules"][0]["pending_reload"]["spec"]["status"],
        "match"
    );
    first_module.stop().await.unwrap();
    second_module.stop().await.unwrap();
}

/// What an operator is told about a module that speaks no subc wire.
///
/// `live` stays a boolean on the wire, but for this module it answers a weaker
/// question than it does for every other row on the screen: the daemon can say
/// the process it launched is alive and nothing more, because there is no
/// registration to check. Rendering that as the same `running` an ordinary
/// module gets would quietly upgrade the claim, so the gap is named instead --
/// beside the declaration that explains it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_names_the_protocol_and_refuses_to_render_live_as_a_boolean() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module_id = "nats";
    let mut spec = stub_spec_with_env(module_id, vec![("FAKE_AFT_NEVER_CONNECT", "1")]);
    spec.protocol = ModuleProtocol::None;
    let module = supervisor.spawn(spec).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.protocol == ModuleProtocol::None && entry.live
    })
    .await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", module_id, "--verbose"],
    );
    assert_exit(&output, 0);
    let rendered = text(&output.stdout);
    assert!(
        rendered.contains("\n  protocol: none\n"),
        "the declaration must appear on its own line: {rendered}"
    );
    assert!(
        rendered.contains("n/a (no protocol)"),
        "live must not render as a liveness word for a module with no wire: {rendered}"
    );
    assert!(
        !rendered.contains("supervision: enabled \u{b7} running"),
        "live rendered as if the daemon had checked a registration it never had: {rendered}"
    );

    let listed = ck_with_subc(
        &server.connection_file_path,
        ["module", "list", "--verbose"],
    );
    assert_exit(&listed, 0);
    let listed = text(&listed.stdout);
    assert!(
        listed.contains("n/a (no protocol)"),
        "the list's live column must carry the same caveat as status: {listed}"
    );

    // `ck --json` is the machine surface and stays the wire verbatim: the
    // renderer's caveat is a rendering, never a rewrite of the field.
    let status_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "status", module_id, "--json"],
    ));
    assert_eq!(status_json["module"]["protocol"], "none");
    assert_eq!(status_json["module"]["live"], true);
    assert_eq!(status_json["module"]["launch_nonce_env"], !cfg!(unix));

    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_reports_launch_nonce_env_false() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "nonce-policy-status";
    let mut spec = stub_spec(module_id);
    for name in ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"] {
        spec.env.push((
            name.to_string(),
            server.temp_dir.join(name).display().to_string(),
        ));
    }
    let module = supervisor.spawn(spec).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| entry.live).await;
    let output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", module_id],
    );
    assert_exit(&output, 0);
    assert!(text(&output.stdout).contains("\n  launch_nonce_env: false\n"));
    let status = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "status", module_id, "--json"],
    ));
    assert_eq!(status["module"]["launch_nonce_env"], false);
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_renders_key_value_block_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "aft",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let status_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "aft", "--json"],
    ));
    assert_eq!(status_json["module"]["module_id"], "aft");
    assert_eq!(
        status_json["module"]["pending_reload"]["spec"]["status"],
        "match"
    );
    assert_eq!(status_json["health"]["status"], "degraded");
    if cfg!(windows) {
        assert_eq!(status_json["module"]["launch_nonce_source"], "pipe");
    }

    let provenance = control_rpc_value_on_stream_within(
        &mut wait_for_client(&server.connection_file_path).await,
        92,
        ClientControlRequest::SupervisorProvenance {
            module_id: Some("aft".to_string()),
        },
        Duration::from_secs(120),
    )
    .await;
    let observed = &provenance["modules"][0]["daemon_observed"];
    let pid = observed["pid"].as_u64().unwrap();
    let started = age_from_ms(observed["spawned_at_ms"].as_u64().unwrap());
    // `ck` shortens paths under its own home folder to `~`. Every test `ck` runs
    // with a throwaway HOME (see isolate_ck_command), so the build folder is never
    // under it and the path is printed in full.
    let binary = observed["spawned_from"].as_str().unwrap().to_string();
    let image = match observed["running_image"]["status"].as_str() {
        Some("match") => "running image matches".to_string(),
        Some("mismatch") => "running image differs: running vs disk".to_string(),
        Some("unavailable") => format!(
            "running image differs: {}",
            observed["running_image"]["reason"]
                .as_str()
                .unwrap_or("none")
        ),
        status => format!("running image status {}", status.unwrap_or("unknown")),
    };

    let output = ck_with_subc(&server.connection_file_path, ["module", "status", "aft"]);
    assert_exit(&output, 0);
    // The age is computed twice from two clocks a few hundred milliseconds
    // apart, so on a slow runner `5s ago` becomes `6s ago` across the second
    // boundary; the rendered age is checked for form, everything else for
    // bytes.
    let rendered = text(&output.stdout);
    let (before, after) = rendered
        .split_once(" · started ")
        .expect("status line carries a start age");
    let (rendered_age, rest) = after
        .split_once(" · restarts ")
        .expect("start age is followed by the restart budget");
    assert!(
        looks_like_age(rendered_age) && looks_like_age(&started),
        "start age renders as an age: {rendered_age:?} vs {started:?}"
    );
    assert_eq!(before, format!("aft — running, degraded\n  pid {pid}"));
    let image_verdict = if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
        "matches file at spawned path"
    } else {
        "not checked on this platform (unsupported_platform)"
    };
    // The resources line carries live memory and CPU figures that change from
    // read to read, so it too is checked for form and cut out before the
    // byte comparison of everything around it.
    let (policy, rest) = rest
        .split_once("\n  resources: ")
        .expect("status carries a resources line after the policy line");
    let (resources, rest) = rest
        .split_once('\n')
        .expect("resources line is followed by more status");
    if cfg!(windows) {
        let (memory, cpu) = resources
            .split_once(" windows working set · cpu ")
            .expect("a Windows resources line names the working-set measurement");
        assert!(memory.starts_with("memory "), "{resources:?}");
        assert!(
            cpu.contains(" (user ") && cpu.contains(", kernel ") && cpu.ends_with(')'),
            "{resources:?}"
        );
    } else if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert!(
            resources.starts_with("memory ") && resources.contains(" · cpu "),
            "a running module renders its memory and cpu: {resources:?}"
        );
    } else {
        assert_eq!(resources, "unavailable (unsupported platform)");
    }
    let rest = format!("{policy}\n{rest}");
    // The budget renders with the window it is counted over (`in 10m`), because
    // the count alone reads as a lifetime total and stopped being one.
    assert_eq!(
        rest,
        format!(
            "0 of 1 in 10m · drain 25 ms · restart backoff 10 ms to 30s\n  launch_nonce_env: {nonce_env}{nonce_source}\n  last exit: none\n  drain gauges: 0 drains with undeclared gauge\n  binary: {binary} ({image})\n  configured program: matches running process\n  running image: {image_verdict}\n  launch spec: matches running process\n",
            nonce_env = !cfg!(unix),
            nonce_source = if cfg!(windows) { "\n  launch nonce source (daemon): pipe" } else { "" },
        )
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_terminals_renders_empty_history_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub(&server, &supervisor, "aft").await;
    let json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "terminals", "aft", "--json"],
    ));
    let started = age_from_ms(json["daemon_started_at_ms"].as_u64().unwrap());

    let output = ck_with_subc(&server.connection_file_path, ["module", "terminals", "aft"]);
    assert_exit(&output, 0);
    // Same two-clock age as the status test: form-checked, not byte-matched.
    let rendered = text(&output.stdout);
    let age = rendered
        .strip_prefix("no retained exits (current daemon started ")
        .and_then(|rest| rest.strip_suffix(")\n"))
        .expect("one sentence naming the daemon start age");
    assert!(
        looks_like_age(age) && looks_like_age(&started),
        "start age renders as an age: {age:?} vs {started:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_terminals_renders_one_record_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_restart_limit(&server, 0);
    let module = supervisor
        .spawn(stub_spec_with_env("aft", vec![("FAKE_AFT_EXIT_CODE", "7")]))
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, "aft", |entry| {
        entry.state == "failed" && !entry.live
    })
    .await;
    let json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "terminals", "aft", "--json"],
    ));
    let entry = &json["entries"][0];
    assert_eq!(json["entries"].as_array().unwrap().len(), 1);
    let at_ms = entry["at_ms"].as_u64().unwrap();
    let fixed_now_ms = at_ms - 7 * 24 * 60 * 60 * 1_000;
    let disposition = entry["disposition"]
        .as_str()
        .unwrap()
        .replace(['_', '-'], " ");

    let output = ck_with_subc_at_now(
        &server.connection_file_path,
        ["module", "terminals", "aft"],
        fixed_now_ms,
    );
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        single_row_table(
            ["when", "exit", "disposition", "daemon"],
            ["in 7d", "exit 7", &disposition, "-"]
        )
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_stop_start_json_drive_supervisor() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-control-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let restart = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "restart", module_id, "--json"],
    ));
    assert_eq!(restart["module_id"], module_id);
    assert_eq!(restart["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    let stop = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "stop", module_id, "--json"],
    ));
    assert_eq!(stop["module_id"], module_id);
    assert_eq!(stop["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "disabled" && !entry.enabled && !entry.live
    })
    .await;

    let start = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "start", module_id, "--json"],
    ));
    assert_eq!(start["module_id"], module_id);
    assert_eq!(start["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_stop_waits_past_ten_seconds_within_running_drain_budget() {
    let server = TestServer::start().await;
    let module_id = "ck-long-real-drain";
    let drain_timeout = Duration::from_millis(14_000);
    let supervisor = Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(3, Duration::from_millis(137))
            .with_max_backoff(Duration::from_millis(7_321)),
    )
    .with_privacy_trampoline(common::ckdev_subc())
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_forwarding(Arc::clone(&server.forwarding))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(drain_timeout)
    .with_connection_file_path(server.connection_file_path.clone());
    let events_path = server.temp_dir.join("ck-long-real-drain-events.jsonl");
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        vec![
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            ("FAKE_AFT_EVENTS_PATH", events_path.to_str().unwrap()),
        ],
    )
    .await;
    let entry = wait_for_supervisor_entry(&server.connection_file_path, module_id, |_| true).await;
    assert_eq!(entry.drain_timeout_ms, Some(14_000));

    let mut route_client = wait_for_client(&server.connection_file_path).await;
    let route = open_tool_route(&mut route_client, module_id, 45_000).await;
    let request_corr = 45_001;
    let request = serde_json::to_vec(&json!({
        "uncancellable": true,
        "delay_ms": 12_500
    }))
    .unwrap();
    write_frame(
        &mut route_client,
        &data_request(route, request_corr, &request),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    wait_for_stub_request(&events_path, route.channel, request_corr).await;

    let started = Instant::now();
    let stop = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "stop", module_id, "--json"],
    ));
    assert!(
        started.elapsed() > Duration::from_secs(10),
        "the real drain must cross the former flat 10s deadline"
    );
    assert_eq!(stop["module_id"], module_id);
    assert_eq!(stop["applied"], true);
    let status = module.status().unwrap();
    assert!(!status.enabled && !status.live);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_start_stop_print_exact_drift_lines_and_preserve_matching_bytes() {
    for enabled in [true, false] {
        for configured_enabled in [None, Some(false), Some(true)] {
            for listed in [true, false] {
                for json_output in [false, true] {
                    let module_id = "drift";
                    let verb = if enabled { "start" } else { "stop" };
                    let state = if enabled { "running" } else { "disabled" };
                    let entry = SupervisorEntry::new(
                        module_id,
                        state,
                        enabled,
                        enabled,
                        SupervisorHealthStatus::Unknown,
                    )
                    .with_configured_enabled(configured_enabled);
                    let before = ClientControlResponse::SupervisorList {
                        generation: 1,
                        modules: vec![entry
                            .clone()
                            .with_configured_enabled(configured_enabled.map(|value| !value))],
                    };
                    let unrelated = SupervisorEntry::new(
                        "another-module",
                        state,
                        enabled,
                        enabled,
                        SupervisorHealthStatus::Unknown,
                    )
                    .with_configured_enabled(Some(!enabled));
                    let mut modules = vec![unrelated];
                    if listed {
                        modules.push(entry);
                    }
                    let after = ClientControlResponse::SupervisorList {
                        generation: 1,
                        modules,
                    };
                    let daemon = ScriptedDaemon::start(
                        "ck-enabled-drift",
                        vec![
                            ScriptedControlStep {
                                expected: ClientControlRequest::SupervisorList {},
                                action: ScriptedControlAction::Reply(Box::new(before)),
                            },
                            ScriptedControlStep {
                                expected: ClientControlRequest::SupervisorSetEnabled {
                                    module_id: module_id.into(),
                                    enabled,
                                },
                                action: ScriptedControlAction::Reply(Box::new(
                                    ClientControlResponse::SupervisorAck {
                                        module_id: module_id.into(),
                                        applied: true,
                                    },
                                )),
                            },
                            ScriptedControlStep {
                                expected: ClientControlRequest::SupervisorList {},
                                action: ScriptedControlAction::Reply(Box::new(after)),
                            },
                        ],
                    )
                    .await;
                    let output = if json_output {
                        ck_with_subc(
                            &daemon.connection_file_path,
                            ["module", verb, module_id, "--json"],
                        )
                    } else {
                        ck_with_subc(&daemon.connection_file_path, ["module", verb, module_id])
                    };
                    assert_exit(&output, 0);
                    if json_output {
                        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
                        assert!(body.get("configured_enabled").is_none());
                        assert_eq!(
                            body["module"].get("configured_enabled"),
                            configured_enabled
                                .filter(|_| listed)
                                .map(Value::Bool)
                                .as_ref()
                        );
                        continue;
                    }
                    let notice = match (enabled, configured_enabled.filter(|_| listed)) {
                    (true, Some(false)) => "started; subc.jsonc has \"enabled\": false, so the next rescan or daemon restart stops it. Set \"enabled\": true to keep it running.\n",
                    (false, Some(true)) => "stopped; subc.jsonc has \"enabled\": true, so the next rescan or daemon restart starts it again. Set \"enabled\": false to keep it stopped.\n",
                    _ => "",
                };
                    assert_eq!(
                        text(&output.stdout),
                        format!(
                            "module  applied  {}\ndrift   true     {state}\ndaemon: {}\n{notice}",
                            if !listed {
                                "state"
                            } else if enabled {
                                "state  "
                            } else {
                                "state   "
                            },
                            daemon.connection_file_path.display(),
                            state = if listed { state } else { "-    " }
                        )
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_timeout_exits_outcome_unknown_and_names_status_verification() {
    let module_id = "scripted-stop-timeout";
    let daemon = ScriptedDaemon::start(
        "ck-mutating-timeout",
        vec![
            ScriptedControlStep {
                expected: ClientControlRequest::SupervisorList {},
                action: ScriptedControlAction::Reply(Box::new(
                    ClientControlResponse::SupervisorList {
                        generation: 1,
                        modules: vec![scripted_supervisor_entry(module_id, Some(0))],
                    },
                )),
            },
            ScriptedControlStep {
                expected: ClientControlRequest::SupervisorSetEnabled {
                    module_id: module_id.to_string(),
                    enabled: false,
                },
                action: ScriptedControlAction::NeverReply,
            },
        ],
    )
    .await;

    let output = ck_with_subc(&daemon.connection_file_path, ["module", "stop", module_id]);
    assert_exit(&output, 4);
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(&format!("outcome unknown: `ck module stop {module_id}`")),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("verify with: ck module status {module_id}")),
        "stderr:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_timeout_remains_failure_not_outcome_unknown() {
    let daemon = ScriptedDaemon::start(
        "ck-read-only-timeout",
        vec![ScriptedControlStep {
            expected: ClientControlRequest::SupervisorList {},
            action: ScriptedControlAction::NeverReply,
        }],
    )
    .await;

    let output = ck_with_subc(&daemon.connection_file_path, ["module", "list"]);
    assert_exit(&output, 1);
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert_eq!(stderr, "timed out after 10s waiting for a frame\n");
    assert!(!stderr.contains("outcome unknown"));
}

#[test]
fn module_help_documents_failed_and_outcome_unknown_exit_codes() {
    let output = ck_command().args(["module"]).output().unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("1  operation refused or failed"));
    assert!(stdout.contains("4  mutating operation timed out; outcome unknown"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_empty_result_has_a_next_step_and_json_has_no_footer() {
    let server = TestServer::start().await;

    let output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("no supervised modules"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
    assert!(stdout.contains("ck module rescan"), "stdout:\n{stdout}");

    let json_output = ck_with_subc(&server.connection_file_path, ["module", "list", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_empty_result_is_one_line_and_json_has_no_footer() {
    let server = TestServer::start().await;

    let output = ck_with_subc(&server.connection_file_path, ["routes"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "no live routes\n");

    let json_output = ck_with_subc(&server.connection_file_path, ["routes", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );
}

#[test]
fn provenance_requires_exactly_one_module_id_without_connecting() {
    for args in [
        vec!["provenance"],
        vec!["provenance", "--help"],
        vec!["provenance", "aft", "extra"],
    ] {
        let output = ck_command().args(args).output().unwrap();
        assert_exit(&output, 0);
        let stdout = text(&output.stdout);
        assert!(stdout.contains("ck provenance"), "stdout:\n{stdout}");
        assert!(stdout.contains("usage:"), "stdout:\n{stdout}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_json_preserves_the_complete_daemon_response_without_a_footer() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_BUILD_COMMIT", "declared-commit"),
            ("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock"),
            ("FAKE_AFT_WIRE_CRATE_VERSION", "declared-wire"),
            ("FAKE_AFT_STORE_SCHEMA_VERSION", "declared-schema"),
        ],
    )
    .await;

    // First provenance evaluation pays the linux double-hash of the stub
    // binary; see control_rpc_value_on_stream_within for why 10s is not enough
    // on a contended CI disk.
    let expected = control_rpc_value_on_stream_within(
        &mut wait_for_client(&server.connection_file_path).await,
        91,
        ClientControlRequest::SupervisorProvenance {
            module_id: Some("aft".to_string()),
        },
        Duration::from_secs(120),
    )
    .await;
    let output = ck_with_subc(
        &server.connection_file_path,
        ["--json", "provenance", "aft"],
    );
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, expected);
    assert!(stdout.contains("\"module_declared\""), "stdout:\n{stdout}");
    assert!(stdout.contains("\"daemon_observed\""), "stdout:\n{stdout}");
    assert!(!stdout.contains("next:"), "stdout:\n{stdout}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_human_output_keeps_declared_values_under_the_declared_label() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_BUILD_COMMIT", "declared-dirty-dirty"),
            ("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock"),
            ("FAKE_AFT_WIRE_CRATE_VERSION", "declared-wire"),
            ("FAKE_AFT_STORE_SCHEMA_VERSION", "declared-schema"),
        ],
    )
    .await;

    let output = ck_with_subc(&server.connection_file_path, ["provenance", "aft"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let declared_at = stdout
        .find("Module declared")
        .unwrap_or_else(|| panic!("stdout:\n{stdout}"));
    let observed_at = stdout
        .rfind("Daemon observed")
        .unwrap_or_else(|| panic!("stdout:\n{stdout}"));
    assert!(declared_at < observed_at, "stdout:\n{stdout}");
    let module_declared = &stdout[declared_at..observed_at];
    let module_observed = &stdout[observed_at..];
    let expected_observed_start = if cfg!(windows) {
        "Daemon observed\n  launch nonce source: pipe\n  pid:"
    } else {
        "Daemon observed\n  pid:"
    };
    assert!(
        module_observed.starts_with(expected_observed_start),
        "module-level observed section boundary was not verified:\n{module_observed}"
    );
    for declared in [
        "declared-dirty-dirty",
        "declared-lock",
        "declared-wire",
        "declared-schema",
    ] {
        let value_at = module_declared
            .find(declared)
            .unwrap_or_else(|| panic!("missing {declared:?} in:\n{stdout}"));
        assert!(
            value_at < module_declared.len(),
            "declared value {declared:?} escaped its source section:\n{stdout}"
        );
        assert!(
            !module_observed.contains(declared),
            "declared value {declared:?} leaked into module-level observed section:\n{module_observed}"
        );
    }
    // The launch nonce source is shown under the declared section with the same
    // value the JSON form carries, so an owner can read it without `--json`.
    let json_output = ck_with_subc(
        &server.connection_file_path,
        ["--json", "provenance", "aft"],
    );
    assert_exit(&json_output, 0);
    let json: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let source = json["modules"][0]["module_declared"]["build"]["launch_nonce_source"]
        .as_str()
        .unwrap_or_else(|| panic!("the stub declares a nonce source: {json}"));
    let expected_source = if cfg!(windows) {
        "pipe"
    } else if cfg!(unix) {
        "fd"
    } else {
        "env"
    };
    assert_eq!(source, expected_source);
    assert!(
        module_declared.contains(&format!("launch nonce source: {source}")),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("Daemon build"), "stdout:\n{stdout}");
    assert_eq!(
        stdout.matches("Daemon build").count(),
        1,
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("Module: aft"), "stdout:\n{stdout}");
    assert!(stdout.contains("pid:"), "stdout:\n{stdout}");
    assert!(stdout.contains("started "), "stdout:\n{stdout}");
    assert!(stdout.contains("spawned from:"), "stdout:\n{stdout}");
    assert!(stdout.contains("running image "), "stdout:\n{stdout}");
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    assert!(
        module_observed.contains("running image matches the file it was spawned from"),
        "stdout:\n{stdout}"
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    assert!(
        stdout.contains("running image could not be compared"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commit match only"), "stdout:\n{stdout}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_hostile_declared_values_are_refused_before_rendering() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let _module = supervisor
        .spawn(stub_spec_with_env(
            "aft",
            vec![
                ("FAKE_AFT_BUILD_COMMIT", "\u{1b}]52;c;AAAA\u{07}"),
                ("FAKE_AFT_BUILD_LOCK_DIGEST", "\u{1b}[2J"),
            ],
        ))
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, "aft", |entry| {
        entry.state == "failed" && !entry.live
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_renders_unverifiable_and_lock_only_declarations() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let unverifiable = spawn_stub(&server, &supervisor, "unverifiable").await;
    let lock_only = spawn_stub_with_env(
        &server,
        &supervisor,
        "lock-only",
        vec![("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock-only")],
    )
    .await;

    let absent = ck_with_subc(&server.connection_file_path, ["provenance", "unverifiable"]);
    assert_exit(&absent, 0);
    assert!(
        text(&absent.stdout).contains("unverifiable"),
        "stdout:\n{}",
        text(&absent.stdout)
    );

    let lock_only_output = ck_with_subc(&server.connection_file_path, ["provenance", "lock-only"]);
    assert_exit(&lock_only_output, 0);
    assert!(
        text(&lock_only_output.stdout).contains("change-detectable; commit identity unavailable"),
        "stdout:\n{}",
        text(&lock_only_output.stdout)
    );

    unverifiable.stop().await.unwrap();
    lock_only.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_unknown_module_preserves_the_daemon_error_and_exit_code() {
    let server = TestServer::start().await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["provenance", "missing-module"],
    );
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("unknown_module"), "stderr:\n{stderr}");
    assert!(stderr.contains("missing-module"), "stderr:\n{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_and_health_unknown_module_errors_are_one_line() {
    let server = TestServer::start().await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "missing-module"],
    );
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr),
        "no module named 'missing-module'. Run ck module list.\n"
    );

    let health = ck_with_subc(&server.connection_file_path, ["health", "missing-module"]);
    assert_exit(&health, 1);
    assert_eq!(
        text(&health.stderr),
        "no module named 'missing-module'. Run ck module list.\n"
    );

    let json_output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "missing-module", "--json"],
    );
    let json_stderr = text(&json_output.stderr);
    assert_exit(&json_output, 1);
    assert!(
        !json_stderr.contains("next:"),
        "JSON error gained footer: {json_stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_summarizes_provider_state_and_headline_metrics() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let unconfigured = (0..36)
        .map(|index| format!("provider-{index}"))
        .collect::<Vec<_>>();
    let metrics = serde_json::json!({
        "degraded": ["antigravity"],
        "fetchBlackout": false,
        "lastTickAgeSecs": 2,
        "unconfigured": unconfigured,
    })
    .to_string();
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_METRICS", metrics.as_str()),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let overview_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["health", "--json"],
    ));
    assert_eq!(overview_json["modules"][0]["module_id"], "insula");
    let detail_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["health", "insula", "--json"],
    ));
    assert_eq!(detail_json["metrics"]["lastTickAgeSecs"], 2);

    let overview = ck_with_subc(&server.connection_file_path, ["health"]);
    assert_exit(&overview, 0);
    assert_eq!(
        text(&overview.stdout),
        "insula  degraded  1 provider degraded (antigravity); 36 not configured\n"
    );

    let detail = ck_with_subc(&server.connection_file_path, ["health", "insula"]);
    assert_exit(&detail, 0);
    assert_eq!(
        text(&detail.stdout),
        "insula: degraded\n  1 provider degraded (antigravity); 36 not configured\n  fetchBlackout: disabled\n  lastTickAgeSecs: 2s ago\n"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_shows_healthy_health_detail_and_headline_metrics() {
    let server = TestServer::start_named("ckdev-module-status-health").await;
    let supervisor = supervisor_with_fast_health(&server);
    let xdg_root = server.temp_dir.path().join("child-xdg");
    let xdg_paths = [
        ("XDG_DATA_HOME", xdg_root.join("data")),
        ("XDG_RUNTIME_DIR", xdg_root.join("runtime")),
        ("XDG_CONFIG_HOME", xdg_root.join("config")),
    ];
    for (_, path) in &xdg_paths {
        fs::create_dir_all(path).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&xdg_paths[1].1, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let xdg_data = xdg_paths[0].1.to_str().unwrap();
    let xdg_runtime = xdg_paths[1].1.to_str().unwrap();
    let xdg_config = xdg_paths[2].1.to_str().unwrap();

    let metrics = serde_json::json!({
        "headline": ["indexing"],
        "indexing": "view publication prefrontal 0/? files [preparing]",
        "roots_warming": 84,
    })
    .to_string();
    let with_health = spawn_stub_with_env(
        &server,
        &supervisor,
        "ckdev-status-detail",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "ok"),
            (
                "FAKE_AFT_HEALTH_DETAIL",
                "indexing view publication prefrontal 0/? files [preparing]",
            ),
            ("FAKE_AFT_HEALTH_METRICS", metrics.as_str()),
            ("XDG_DATA_HOME", xdg_data),
            ("XDG_RUNTIME_DIR", xdg_runtime),
            ("XDG_CONFIG_HOME", xdg_config),
        ],
    )
    .await;
    let without_health_detail = spawn_stub_with_env(
        &server,
        &supervisor,
        "ckdev-status-empty",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("XDG_DATA_HOME", xdg_data),
            ("XDG_RUNTIME_DIR", xdg_runtime),
            ("XDG_CONFIG_HOME", xdg_config),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "ckdev-status-detail",
        SupervisorHealthStatus::Ok,
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "ckdev-status-empty",
        SupervisorHealthStatus::Ok,
    )
    .await;

    let status = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "ckdev-status-detail"],
    );
    assert_exit(&status, 0);
    let stdout = text(&status.stdout);
    assert!(
        stdout.contains("  indexing view publication prefrontal 0/? files [preparing]\n"),
        "healthy detail missing from module status:\n{stdout}"
    );
    assert!(
        stdout.contains("  indexing: view publication prefrontal 0/? files [preparing]\n"),
        "headline metric missing from module status:\n{stdout}"
    );
    assert!(
        stdout.contains("metrics: run `ck health ckdev-status-detail`\n"),
        "the non-headline metrics hint should remain:\n{stdout}"
    );

    let empty = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "ckdev-status-empty"],
    );
    assert_exit(&empty, 0);
    let empty_stdout = text(&empty.stdout);
    assert!(
        !empty_stdout.contains("metrics: run `ck health ckdev-status-empty`"),
        "status without extra metrics should not print the health hint:\n{empty_stdout}"
    );
    assert!(
        !empty_stdout.contains("indexing view publication prefrontal 0/? files [preparing]"),
        "status without a health detail should omit its detail line:\n{empty_stdout}"
    );
    assert!(
        !empty_stdout.contains("indexing:"),
        "status without headline metrics should omit their lines:\n{empty_stdout}"
    );
    assert!(
        !empty_stdout.contains("  metrics: none\n"),
        "status without metrics should omit a headline line:\n{empty_stdout}"
    );

    with_health.stop().await.unwrap();
    without_health_detail.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_empty_result_has_a_next_step_and_json_has_no_footer() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let fixture = serde_json::json!([]);
    let module = spawn_quota_stub(&server, &supervisor, "insula", &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("no providers reported"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("ck module status <module-id> --subc <connection-file>"),
        "stdout:\n{stdout}"
    );

    let json_output = ck_with_subc(&server.connection_file_path, ["quota", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_reports_local_provider_not_running_in_one_line() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let fixture = serde_json::json!([{
        "provider": "antigravity",
        "account": "local",
        "errorClass": "local_source_unavailable",
        "error": "no Antigravity language server or agy CLI process running"
    }]);
    let module = spawn_quota_stub(&server, &supervisor, "insula", &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "Antigravity — not running locally\n");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_table_renders_providers_and_used_percent() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Anthropic"), "stdout:\n{stdout}");
    assert!(stdout.contains("Openai"), "stdout:\n{stdout}");
    assert!(stdout.contains("work"), "account label, stdout:\n{stdout}");
    // The breakdown layout renders utilization as "42% used" in the window
    // detail string beside the progress bar.
    assert!(
        stdout.contains("42% used"),
        "expected usedPercent 42.0 in window details, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Bonus credits"),
        "extraRateWindows title should appear, stdout:\n{stdout}"
    );
    // The default view lists connected providers only; entries without a
    // usage object collapse into the summary line.
    assert!(
        stdout.contains("1 providers not configured"),
        "not-connected summary should appear, stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("cookie jar unreadable"),
        "error-only entry must stay out of the default view, stdout:\n{stdout}"
    );

    let verbose = ck_with_subc(&server.connection_file_path, ["quota", "--verbose"]);
    assert_exit(&verbose, 0);
    let verbose_stdout = text(&verbose.stdout);
    assert!(
        verbose_stdout.contains("rate limit probe failed for secondary account"),
        "degraded error should appear under --verbose, stdout:\n{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("cookie jar unreadable"),
        "error-only entry should appear under --verbose, stdout:\n{verbose_stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_filters_by_provider_id() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "anthropic"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Anthropic"), "stdout:\n{stdout}");
    // Provider sections are headed by the display name; the filtered view
    // must not contain the other provider's section at all.
    assert!(
        !stdout.contains("Openai"),
        "filtered view leaked openai section, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("42% used"),
        "expected usedPercent 42.0, stdout:\n{stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_unknown_provider_lists_valid_ids_and_exits_nonzero() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["quota", "unknown-id", "--verbose"],
    );
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("unknown provider 'unknown-id'"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("anthropic"), "stderr:\n{stderr}");
    assert!(stderr.contains("openai"), "stderr:\n{stderr}");
    assert!(stderr.contains("grok"), "stderr:\n{stderr}");
    assert!(stderr.contains("next:"), "stderr:\n{stderr}");
    assert!(stderr.contains("ck quota --verbose"), "stderr:\n{stderr}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_json_emits_wrapped_reply_verbatim() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "--json"]);
    let value = assert_json_success(output);
    let result = value["result"].as_array().expect("result array");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0]["provider"], "anthropic");
    assert!(result[0]["usage"]["primary"]["usedPercent"]
        .as_f64()
        .is_some());
    assert_eq!(result[2]["error"].as_str(), Some("cookie jar unreadable"));

    module.stop().await.unwrap();
}

#[test]
fn discovery_failure_lists_tried_paths_and_exits_2() {
    let runtime = TempDir::new("ck-empty-runtime");
    let home = TempDir::new("ck-empty-home");
    let tmp = TempDir::new("ck-empty-tmp");

    let output = ck_command()
        .args(["module", "list", "--json"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .env("TMP", tmp.path())
        .env("TEMP", tmp.path())
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(
        output.stdout.is_empty(),
        "discovery failures must not write data to stdout: {}",
        text(&output.stdout)
    );
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "stderr should be one line: {stderr}"
    );
    assert!(stderr.contains("no usable subc connection file found"));
    assert!(stderr.contains(
        &runtime
            .path()
            .join("subc-connection.json")
            .display()
            .to_string()
    ));
    assert!(stderr.contains(&home_connection_file(home.path()).display().to_string()));
    assert!(stderr.contains(&tmp.path().display().to_string()));

    let text_output = ck_command()
        .args(["module", "list"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .env("TMP", tmp.path())
        .env("TEMP", tmp.path())
        .output()
        .unwrap();
    assert_exit(&text_output, 2);
    let text_stderr = text(&text_output.stderr);
    assert!(text_stderr.contains("next:"), "stderr:\n{text_stderr}");
    assert!(
        text_stderr.contains("ck daemon --subc <connection-file>"),
        "stderr:\n{text_stderr}"
    );
}

/// The age vocabulary `ck` renders (`just now`, `5s ago`, `4h ago`), as
/// opposed to a raw timestamp leaking through.
fn looks_like_age(text: &str) -> bool {
    text == "just now" || text.ends_with(" ago")
}

/// The test CLI, run as `ckdev-under-test` (see `common::ck_under_test_command`).
fn ck_command() -> Command {
    let mut command = common::ck_under_test_command();
    isolate_ck_command(&mut command);
    command
}

#[cfg(unix)]
fn direct_ck_command() -> Command {
    // Setup fixtures use isolated user paths, so they can run Cargo's
    // `ck-under-test` binary without publishing `ckdev-under-test` into
    // `/tmp/cortexkit-ckdev-<uid>`.
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-under-test"));
    isolate_ck_command(&mut command);
    command
}

fn isolate_ck_command(command: &mut Command) {
    let home = kept_temp_dir("ck-home");
    let temp = kept_temp_dir("ck-tmp");
    // Every CLI test gets an isolated update cache and a closed local endpoint.
    // This proves dashboard output without reaching public release infrastructure.
    // Domain discovery checks the managed bin directory and then PATH, so a
    // test that pins rendered output must not inherit a host data home or user
    // PATH containing real domains. Every CLI test therefore runs with the
    // SYSTEM path only: `curl` and `sh` stay reachable (the update check shells
    // out, and a ck that cannot spawn curl never connects to a test's listener,
    // which wedges that test in `accept()` rather than failing it), while the
    // user-level directories that hold installed `ck-*` binaries are absent.
    // A test that wants a domain builds one in its own directory and prepends it.
    // The temp directory is ck's own too: connection-file discovery's last
    // candidate lives there, and it must not point to a daemon outside the test.
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMPDIR", &temp)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env(
            "CK_UPDATE_CACHE_PATH",
            kept_temp_dir("ck-update-cache").join("update-metadata.json"),
        )
        .env("XDG_DATA_HOME", kept_temp_dir("ck-data-home"))
        .env("XDG_CONFIG_HOME", kept_temp_dir("ck-config-home"))
        .env("XDG_RUNTIME_DIR", kept_temp_dir("ck-runtime-dir"))
        .env("LOCALAPPDATA", kept_temp_dir("ck-local-app-data"))
        .env("CK_PROFILE_PATH", home.join(".profile"))
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/index.json")
        .env("PATH", system_path_only());
}

/// A scratch folder that stays on disk until this test process exits.
/// `isolate_ck_command` points a command at folders the command uses after
/// the function returns, so their guards are kept here rather than dropped,
/// which would delete the folders first. They live under this process's
/// scratch root, which a later test run removes once this process is gone.
fn kept_temp_dir(name: &str) -> PathBuf {
    static KEPT: std::sync::Mutex<Vec<TempDir>> = std::sync::Mutex::new(Vec::new());
    let dir = unique_temp_dir(name);
    let path = dir.path().to_path_buf();
    KEPT.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(dir);
    path
}

/// The platform's system tool directories and nothing else.
///
/// On Windows the release-index transport is `powershell.exe`, which lives
/// under `System32\WindowsPowerShell\v1.0`, not `System32` itself; without
/// that entry every setup and upgrade test fails with "program not found",
/// and the hanging-source test never connects, so its listener thread waits
/// on `accept()` forever and the whole suite hangs on the join.
fn system_path_only() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let root = Path::new(&root);
        std::env::join_paths([
            root.join("System32"),
            root.to_path_buf(),
            root.join("System32").join("WindowsPowerShell").join("v1.0"),
        ])
        .unwrap()
    }
    #[cfg(not(windows))]
    {
        "/usr/bin:/bin:/usr/sbin:/sbin".into()
    }
}

fn ck_with_subc<const N: usize>(connection_file: &Path, args: [&str; N]) -> Output {
    let mut command = ck_command();
    command.args(args).arg("--subc").arg(connection_file);
    command.output().unwrap()
}

fn ck_with_subc_at_now<const N: usize>(
    connection_file: &Path,
    args: [&str; N],
    now_ms: u64,
) -> Output {
    let mut command = ck_command();
    command
        .env("CK_TEST_TERMINALS_NOW_MS", now_ms.to_string())
        .args(args)
        .arg("--subc")
        .arg(connection_file);
    command.output().unwrap()
}

fn assert_json_success(output: Output) -> Value {
    assert_exit(&output, 0);
    assert!(
        output.stderr.is_empty(),
        "JSON command wrote stderr: {}",
        text(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout was not JSON: {error}: {}", text(&output.stdout)));
    let expected = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    assert_eq!(
        text(&output.stdout),
        expected,
        "--json bytes changed from the captured pretty-JSON renderer"
    );
    value
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected ck exit status\nstdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn home_connection_file(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("cortexkit")
        .join("run")
        .join("subc-connection.json")
}

fn supervisor(server: &TestServer) -> Supervisor {
    supervisor_with_restart_limit(server, 1)
}

fn supervisor_with_restart_limit(server: &TestServer, max_restarts: u32) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, Duration::from_millis(10)),
    )
    .with_privacy_trampoline(common::ckdev_subc())
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_forwarding(Arc::clone(&server.forwarding))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

fn supervisor_with_fast_health(server: &TestServer) -> Supervisor {
    supervisor(server).with_health_config(HealthConfig {
        cadence: Duration::from_millis(10),
        deadline: Duration::from_secs(1),
        ..HealthConfig::default()
    })
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
) -> SupervisedModule {
    let module = supervisor.spawn(stub_spec(module_id)).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

async fn spawn_stub_with_env(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    env: Vec<(&str, &str)>,
) -> SupervisedModule {
    let mut spec = stub_spec(module_id);
    spec.env.extend(
        env.into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    let module = supervisor.spawn(spec).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

fn stub_spec(module_id: &str) -> ModuleSpec {
    stub_spec_with_env(module_id, Vec::new())
}

fn stub_spec_with_env(module_id: &str, env: Vec<(&str, &str)>) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env: std::iter::once(("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()))
            .chain(
                env.into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            )
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
    }
}

#[derive(Clone, Copy)]
struct CliTestRoute {
    channel: u16,
    epoch: u32,
}

async fn open_tool_route<S>(stream: &mut S, module_id: &str, corr: u64) -> CliTestRoute
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let response = control_rpc_on_stream(
        stream,
        corr,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity::new(
                std::env::current_dir().unwrap(),
                "ck-deadline-test".to_string(),
                format!("session-{corr}"),
            ),
            consumer_identity: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
        },
    )
    .await;
    match response {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } => CliTestRoute {
            channel: route_channel,
            epoch: route_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

fn data_request(route: CliTestRoute, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        route.channel,
        route.epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

async fn wait_for_stub_request(path: &Path, channel: u16, corr: u64) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let received = fs::read_to_string(path)
            .ok()
            .into_iter()
            .flat_map(|contents| {
                contents
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .collect::<Vec<_>>()
            })
            .any(|event| {
                event["kind"] == "request_received"
                    && event["channel"].as_u64() == Some(u64::from(channel))
                    && event["corr"].as_u64() == Some(corr)
            });
        if received {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stub did not receive channel {channel} corr {corr} within {SETUP_TIMEOUT:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

fn scripted_supervisor_entry(module_id: &str, drain_timeout_ms: Option<u64>) -> SupervisorEntry {
    SupervisorEntry::new(module_id, "running", true, true, SupervisorHealthStatus::Ok)
        .with_protocol(ModuleProtocol::Subc)
        .with_restart_count(Some(0))
        .with_max_restarts(Some(3))
        .with_lifetime_restarts(Some(0))
        .with_spawn_generation(Some(1))
        .with_restart_window_secs(Some(600))
        .with_drain_timeout_ms(drain_timeout_ms)
        .with_restart_backoff_ms(Some(100))
        .with_restart_max_backoff_ms(Some(30_000))
}

fn quota_wire_fixture() -> Value {
    serde_json::json!([
        {
            "provider": "anthropic",
            "account": "work",
            "source": "vault",
            "usage": {
                "primary": {
                    "usedPercent": 42.0,
                    "resetsAt": "2099-06-15T14:30:00Z",
                    "windowMinutes": 300
                },
                "secondary": {
                    "usedPercent": 17.5,
                    "resetsAt": "2099-06-22T08:00:00+00:00",
                    "windowMinutes": 10080
                },
                "extraRateWindows": [
                    {
                        "title": "Bonus credits",
                        "id": "bonus-1",
                        "window": {
                            "usedPercent": 5.0,
                            "resetsAt": "2099-07-01T00:00:00Z",
                            "windowMinutes": 43200
                        }
                    }
                ]
            }
        },
        {
            "provider": "openai",
            "account": "personal",
            "error": "rate limit probe failed for secondary account",
            "usage": {
                "primary": {
                    "usedPercent": 88.0,
                    "resetsAt": "2099-06-15T18:00:00Z",
                    "windowMinutes": 300
                }
            }
        },
        {
            "provider": "grok",
            "error": "cookie jar unreadable"
        }
    ])
}

async fn spawn_quota_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    fixture: &Value,
) -> SupervisedModule {
    let fixture_json = serde_json::to_string(fixture).unwrap();
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
                (
                    "FAKE_AFT_ROLE".to_string(),
                    "management_surface".to_string(),
                ),
                ("FAKE_AFT_USAGE_GET_FIXTURE".to_string(), fixture_json),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
) -> SupervisorEntry {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut corr = 1_000;
    loop {
        let modules = supervisor_modules(path, corr).await;
        if let Some(entry) = modules
            .into_iter()
            .find(|entry| entry.module_id == module_id && predicate(entry))
        {
            return entry;
        }
        if Instant::now() >= deadline {
            let modules = supervisor_modules(path, corr + 10_000).await;
            panic!("module {module_id} did not reach expected supervisor state within {SETUP_TIMEOUT:?}; modules: {modules:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the daemon counts exactly `expected` authenticated connections.
/// A connection that was just closed still counts until the daemon reads its
/// end of stream, so a test that prints `connected_clients` waits here first;
/// otherwise a slow runner reports one client too many. Reads the count
/// in-process, so waiting opens no connection of its own.
async fn wait_for_connected_clients(server: &TestServer, expected: u64) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let count = server.connected_clients.count();
        if count == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "the daemon still counts {count} connections after {SETUP_TIMEOUT:?}; expected {expected}"
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_health_status(path: &Path, module_id: &str, expected: SupervisorHealthStatus) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut corr = 30_000;
    loop {
        let mut client = wait_for_client(path).await;
        let response =
            control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorHealth {})
                .await;
        if let ClientControlResponse::SupervisorHealth { modules, .. } = response {
            if modules
                .iter()
                .any(|entry| entry.module_id == module_id && entry.status == expected)
            {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not report {expected:?} within {SETUP_TIMEOUT:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_client(path).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn wait_for_client(path: &Path) -> tokio::net::TcpStream {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        match connect_authed_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => {
                panic!("daemon did not accept authenticated client within {SETUP_TIMEOUT:?}: {err}")
            }
        }
    }
}

async fn control_rpc_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match frame.header.ty {
        FrameType::Response => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Error => panic!(
            "control RPC returned error: {:?}",
            serde_json::from_slice::<Value>(&frame.body).unwrap()
        ),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

/// Control RPC helper with a caller-sized reply window. The provenance op is the
/// motivating caller: on linux its first evaluation sha256-hashes the running
/// module executable twice (via /proc and from disk), and a debug-profile stub
/// on a cold, contended CI disk legitimately exceeds the default 10s — the
/// ubuntu leg failed 2 of 3 runs on exactly that. The window is sized against
/// the slowest acceptable progress of the operation, not the median.
async fn control_rpc_value_on_stream_within<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
    reply_window: Duration,
) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = timeout(reply_window, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for control RPC reply");
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    assert_eq!(frame.header.ty, FrameType::Response);
    serde_json::from_slice(&frame.body).unwrap()
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

fn unique_temp_dir(name: &str) -> TempDir {
    TempDir::new(name)
}
