use super::*;

fn reply(expected: ClientControlRequest, value: Value) -> ScriptedControlStep {
    ScriptedControlStep {
        expected,
        action: ScriptedControlAction::Reply(Box::new(serde_json::from_value(value).unwrap())),
    }
}

fn roster(running: bool) -> Value {
    serde_json::to_value(ClientControlResponse::SupervisorList {
        generation: 1,
        modules: vec![SupervisorEntry::new(
            "broca",
            if running { "running" } else { "stopped" },
            running,
            running,
            SupervisorHealthStatus::Ok,
        )
        .with_drain_timeout_ms(Some(5100))],
    })
    .unwrap()
}

fn reading(pid: Option<u32>, spawned_at_ms: u64, health: &str) -> Vec<ScriptedControlStep> {
    let spawn = json!({"op":"supervisor.spawn_snapshot", "cursor":{"daemon_incarnation":"scripted", "seq":1}, "ring_bound":100, "live":pid.map(|pid| vec![json!({"module_id":"broca", "spawn_generation":pid, "pid":pid, "spawned_at_ms":spawned_at_ms})]).unwrap_or_default()});
    vec![
        reply(
            ClientControlRequest::SupervisorSpawnSnapshot {},
            spawn.clone(),
        ),
        reply(
            ClientControlRequest::SupervisorList {},
            roster(pid.is_some()),
        ),
        reply(
            ClientControlRequest::SupervisorHealth {},
            json!({"op":"supervisor.health", "generation":1, "modules":[{"module_id":"broca", "status":health, "detail": if health == "ok" { Value::Null } else { json!("degraded before; health unknown") }, "consecutive_failures":0, "last_probe_ms":spawned_at_ms + 2100}]}),
        ),
        reply(ClientControlRequest::SupervisorSpawnSnapshot {}, spawn),
    ]
}

fn action(verb: &str) -> ClientControlRequest {
    match verb {
        "restart" => ClientControlRequest::SupervisorRestart {
            module_id: "broca".into(),
            drain_timeout_ms: Some(5100),
        },
        _ => ClientControlRequest::SupervisorSetEnabled {
            module_id: "broca".into(),
            enabled: verb == "start",
        },
    }
}

fn ack() -> Value {
    json!({"op":"supervisor.ack", "module_id":"broca", "applied":true})
}

fn steps(verb: &str, follow: bool, inline: bool, health: &str) -> Vec<Vec<ScriptedControlStep>> {
    let mut sender = vec![];
    if verb != "restart" {
        sender.push(reply(ClientControlRequest::SupervisorList {}, roster(true)));
    }
    sender.push(reply(action(verb), ack()));
    if !inline {
        sender.push(reply(
            ClientControlRequest::SupervisorList {},
            roster(verb != "stop"),
        ));
    }
    if !follow {
        return vec![sender];
    }
    let mut reader = vec![reply(
        ClientControlRequest::CatalogList { module_id: None },
        json!({"op":"catalog.list", "generation":1, "modules":[], "subc_ops":[]}),
    )];
    reader.extend(reading(
        if verb == "start" { None } else { Some(100) },
        1000,
        "ok",
    ));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // The scripted spawn is later than ck's action timestamp even on a slow test host.
    reader.extend(reading(
        if verb == "stop" { None } else { Some(222) },
        now + 60_000,
        health,
    ));
    vec![reader, sender]
}

fn command(ckni: bool, path: &Path, verb: &str, flag: Option<&str>) -> (Option<TempDir>, Command) {
    let (link, mut command) = if ckni {
        let (dir, cmd) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
        (Some(dir), cmd)
    } else {
        (None, ck_command())
    };
    command
        .arg("--subc")
        .arg(path)
        .args(["module", verb, "broca"]);
    if verb == "restart" {
        command.args(["--drain-timeout-ms", "5100"]);
    }
    if let Some(flag) = flag {
        command.arg(flag);
    }
    (link, command)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_wait_adds_one_plain_summary_and_no_wait_keeps_ack_for_both_names() {
    for ckni in [false, true] {
        for verb in ["restart", "stop", "start"] {
            for flag in [None, Some("--no-wait"), Some("--wait")] {
                let follow = flag == Some("--wait");
                let daemon = ScriptedDaemon::start_concurrent_sessions(
                    "inline-pipe",
                    steps(verb, follow, false, "ok"),
                )
                .await;
                let (_link, mut cmd) = command(ckni, &daemon.connection_file_path, verb, flag);
                let output = cmd.output().unwrap();
                assert_exit(&output, 0);
                assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
                let stdout = text(&output.stdout);
                assert!(!stdout.contains('\x1b'), "{stdout}");
                assert!(stdout.contains("applied"), "{stdout}");
                let expected = match verb {
                    "stop" => "broca stopped\n",
                    "start" => "broca started · healthy · pid 222\n",
                    _ => "broca restarted · healthy · pid 222\n",
                };
                assert_eq!(stdout.ends_with(expected), follow, "{stdout}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_actions_follow_only_ck_by_default_and_json_never_follows() {
    for ckni in [false, true] {
        for verb in ["restart", "stop", "start"] {
            for flag in [None, Some("--no-wait"), Some("--wait")] {
                let follow = flag == Some("--wait") || (!ckni && flag.is_none());
                let inline = !ckni && follow;
                let daemon = ScriptedDaemon::start_concurrent_sessions(
                    "inline-pty",
                    steps(verb, follow, inline, "ok"),
                )
                .await;
                let (_link, cmd) = command(ckni, &daemon.connection_file_path, verb, flag);
                let output = upgrade_terminal_output(&cmd);
                assert_exit(&output, 0);
                let stdout = text(&output.stdout);
                assert_eq!(stdout.contains("\x1b[2K"), inline, "{stdout}");
                assert_eq!(stdout.contains("✓ broca"), inline, "{stdout}");
                assert_eq!(stdout.contains("applied"), !inline, "{stdout}");
                if inline && verb != "stop" {
                    assert!(stdout.contains("broca waiting for health"), "{stdout}");
                }
            }
            let daemon = ScriptedDaemon::start_concurrent_sessions(
                "inline-json-pty",
                steps(verb, false, false, "ok"),
            )
            .await;
            let (_link, mut cmd) = command(ckni, &daemon.connection_file_path, verb, None);
            cmd.arg("--json");
            let output = upgrade_terminal_output(&cmd);
            assert_exit(&output, 0);
            let json: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(json["applied"], true);
            assert!(!text(&output.stdout).contains('\x1b'));
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_health_from_before_the_spawn_does_not_count_as_in_flight() {
    let catalog = subc_control::CatalogEntry::new("broca", vec![], vec![]).with_self_signals(Some(
        serde_json::from_value(json!([{
            "name":"work", "kind":"busy", "effect":"observe",
            "anchored_to":{"health_gauges":{"gauges":["executing", "queued"]}}
        }]))
        .unwrap(),
    ));
    let busy_reading = |pid, spawned_at_ms, last_probe_ms, executing, queued| {
        let mut sample = reading(Some(pid), spawned_at_ms, "ok");
        sample[2] = reply(
            ClientControlRequest::SupervisorHealth {},
            json!({"op":"supervisor.health", "generation":1, "modules":[{
                "module_id":"broca", "status":"ok", "consecutive_failures":0,
                "last_probe_ms":last_probe_ms,
                "metrics":{"executing":executing, "queued":queued}
            }]}),
        );
        sample
    };
    let mut reader = vec![reply(
        ClientControlRequest::CatalogList { module_id: None },
        serde_json::to_value(ClientControlResponse::CatalogList {
            generation: 1,
            modules: vec![catalog],
            subc_ops: vec![],
        })
        .unwrap(),
    )];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let live_birth = now - 60_000;
    // The census is stable, but the non-zero cached probe belongs to the previous process.
    reader.extend(busy_reading(100, live_birth, live_birth - 1, 2, 1));
    // A current zero proves that the drain was observed, not that the stale three finished.
    reader.extend(busy_reading(100, live_birth, now, 0, 0));
    let replacement_birth = now + 60_000;
    reader.extend(busy_reading(
        222,
        replacement_birth,
        replacement_birth + 2_100,
        0,
        0,
    ));
    let sender = vec![reply(action("restart"), ack())];
    let daemon =
        ScriptedDaemon::start_concurrent_sessions("inline-stale-probe", vec![reader, sender]).await;
    let (_link, cmd) = command(
        false,
        &daemon.connection_file_path,
        "restart",
        Some("--wait"),
    );
    let output = upgrade_terminal_output(&cmd);
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("broca draining · unknown in flight · budget 5.1 s"),
        "{stdout}"
    );
    assert!(stdout.contains("broca draining · 0 in flight"), "{stdout}");
    assert!(!stdout.contains("3 in flight"), "{stdout}");
    assert!(!stdout.contains("runs finished first"), "{stdout}");
    assert!(stdout.contains("✓ broca restarted"), "{stdout}");
    assert!(stdout.contains("pid 222 · healthy after 2.1 s"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_uses_failure_exit_codes_for_both_names() {
    for ckni in [false, true] {
        for health in ["degraded", "failing"] {
            let daemon = ScriptedDaemon::start_concurrent_sessions(
                "inline-failed",
                steps("restart", true, false, health),
            )
            .await;
            let (_link, mut cmd) = command(
                ckni,
                &daemon.connection_file_path,
                "restart",
                Some("--wait"),
            );
            let output = cmd.output().unwrap();
            assert_exit(&output, 1);
            assert!(
                text(&output.stdout).ends_with(&format!("broca restarted · {health} · pid 222\n"))
            );
            assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
        }
    }
}

#[test]
fn wait_json_and_conflicting_flags_are_usage_errors_for_both_names() {
    for ckni in [false, true] {
        for help in [false, true] {
            let (_link, mut cmd) = if ckni {
                let (dir, cmd) = ckni_command(if cfg!(windows) { "ckni.exe" } else { "ckni" });
                (Some(dir), cmd)
            } else {
                (None, ck_command())
            };
            cmd.args(["--wait", "--json"]);
            if help {
                cmd.arg("--help");
            }
            let output = cmd.output().unwrap();
            assert_exit(&output, 2);
            assert!(output.stdout.is_empty());
            assert!(text(&output.stderr).contains("--wait cannot be used with --json"));
        }
        for flags in [["--wait", "--json"], ["--wait", "--no-wait"]] {
            let (_link, mut cmd) = command(ckni, Path::new("missing-connection"), "restart", None);
            let output = cmd.args(flags).output().unwrap();
            assert_exit(&output, 2);
            assert!(output.stdout.is_empty());
            assert!(text(&output.stderr).contains(if flags[1] == "--json" {
                "--wait cannot be used with --json"
            } else {
                "pass either --wait or --no-wait"
            }));
        }
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_stops_watching_without_cancelling_for_both_names() {
    use std::io::{BufRead, Read};
    use std::process::Stdio;
    for ckni in [false, true] {
        let mut sessions = steps("restart", true, false, "ok");
        sessions[0].truncate(5);
        sessions[0].push(ScriptedControlStep {
            expected: ClientControlRequest::SupervisorSpawnSnapshot {},
            action: ScriptedControlAction::NeverReply,
        });
        let daemon = ScriptedDaemon::start_concurrent_sessions("inline-interrupt", sessions).await;
        let (_link, mut cmd) = command(
            ckni,
            &daemon.connection_file_path,
            "restart",
            Some("--wait"),
        );
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut prefix = String::new();
        loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "ack missing: {prefix}"
            );
            prefix.push_str(&line);
            if line.starts_with("restart initiated;") {
                break;
            }
        }
        let signal = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(signal.success());
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_exit(&output, 0);
        assert!(
            rest.contains("Stopped watching; the action was not cancelled."),
            "{prefix}{rest}"
        );
        assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    }
}
