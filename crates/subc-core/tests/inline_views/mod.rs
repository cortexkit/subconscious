use super::*;
use common::terminal::screen as terminal;
use std::io::BufRead;

fn reply(expected: ClientControlRequest, value: Value) -> ScriptedControlStep {
    ScriptedControlStep {
        expected,
        action: ScriptedControlAction::Reply(Box::new(serde_json::from_value(value).unwrap())),
    }
}

fn health_reply() -> Value {
    json!({"op":"supervisor.health", "generation":1, "modules":[
        {"module_id":"broca", "status":"ok", "consecutive_failures":0, "detail":"Serving requests"},
        {"module_id":"engram", "status":"degraded", "consecutive_failures":0, "detail":"Cloud backup 2 days behind · uploading generation 45"}
    ]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_health_at_100_columns_has_styled_summary_and_only_problem_rows() {
    let mut daemon = ScriptedDaemon::start(
        "inline-health",
        vec![reply(
            ClientControlRequest::SupervisorHealth {},
            health_reply(),
        )],
    )
    .await;
    let mut command = ck_command();
    command
        .arg("--subc")
        .arg(&daemon.connection_file_path)
        .arg("health");
    let parser = terminal(&command);
    let screen = parser.screen();
    assert_eq!(screen.contents(), "   ● 1 healthy    ▲ 1 degraded\n   ▲ engram            Cloud backup 2 days behind · uploading generation 45");
    assert_eq!(
        screen.cell(0, 3).unwrap().fgcolor(),
        vt100::Color::Rgb(30, 138, 63)
    );
    assert_eq!(
        screen.cell(0, 18).unwrap().fgcolor(),
        vt100::Color::Rgb(180, 96, 10)
    );
    assert!(screen.cell(1, 5).unwrap().bold());
    daemon.assert_complete().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_module_list_at_100_columns_keeps_columns_and_colors_status_glyphs() {
    let modules = vec![
        SupervisorEntry::new("broca", "running", true, true, SupervisorHealthStatus::Ok),
        SupervisorEntry::new(
            "engram",
            "running",
            true,
            true,
            SupervisorHealthStatus::Degraded,
        ),
        SupervisorEntry::new(
            "insula",
            "stopped",
            false,
            false,
            SupervisorHealthStatus::Unknown,
        ),
    ];
    let value = serde_json::to_value(ClientControlResponse::SupervisorList {
        generation: 1,
        modules,
    })
    .unwrap();
    let mut daemon = ScriptedDaemon::start(
        "inline-modules",
        vec![reply(ClientControlRequest::SupervisorList {}, value)],
    )
    .await;
    let mut command = ck_command();
    command
        .arg("--subc")
        .arg(&daemon.connection_file_path)
        .args(["module", "list"]);
    let parser = terminal(&command);
    let screen = parser.screen();
    assert_eq!(screen.contents(), "module  status     health    reload \nbroca   ● running  ok        unknown\nengram  ▲ running  degraded  unknown\ninsula  ○ stopped  unknown   unknown");
    assert_eq!(
        screen.cell(1, 8).unwrap().fgcolor(),
        vt100::Color::Rgb(30, 138, 63)
    );
    assert_eq!(
        screen.cell(2, 8).unwrap().fgcolor(),
        vt100::Color::Rgb(180, 96, 10)
    );
    daemon.assert_complete().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_json_wins_over_both_inline_views() {
    for (args, request, response) in [
        (
            vec!["health"],
            ClientControlRequest::SupervisorHealth {},
            health_reply(),
        ),
        (
            vec!["module", "list"],
            ClientControlRequest::SupervisorList {},
            json!({"op":"supervisor.list", "generation":1, "modules":[]}),
        ),
    ] {
        let mut daemon = ScriptedDaemon::start_sessions(
            "inline-json",
            vec![
                vec![reply(request.clone(), response.clone())],
                vec![reply(request, response)],
            ],
        )
        .await;
        let mut command = ck_command();
        command
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .arg("--json")
            .args(args);
        let piped = command.output().unwrap();
        assert_exit(&piped, 0);
        let screen = terminal(&command);
        assert_eq!(screen.screen().contents(), text(&piped.stdout).trim_end());
        assert_eq!(
            screen.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
        daemon.assert_complete().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_health_stdout_pipe_exits_zero_with_empty_stderr() {
    for consume_first_line in [false, true] {
        let modules = (0..500).map(|n| json!({"module_id":format!("module-{n:03}"), "status":"ok", "consecutive_failures":0, "detail":"a".repeat(1000)})).collect::<Vec<_>>();
        let mut daemon = ScriptedDaemon::start(
            "closed-health-pipe",
            vec![reply(
                ClientControlRequest::SupervisorHealth {},
                json!({"op":"supervisor.health", "generation":1, "modules":modules}),
            )],
        )
        .await;
        let mut command = ck_command();
        command
            .arg("--subc")
            .arg(&daemon.connection_file_path)
            .arg("health")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        if consume_first_line {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("module-000  ok  "), "{line}");
        }
        // Dropping the only reader is the portable equivalent of `head -1`.
        // More than a pipe buffer of output remains, so a write must see it.
        drop(reader);
        let output = child.wait_with_output().unwrap();
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
        daemon.assert_complete().await;
    }
}
