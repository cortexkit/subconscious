//! Script-facing output for built-in commands.

mod json;
pub(super) use json::JsonOutput;

use serde_json::Value;
use subc_control::ClientControlRequest;

use super::{CkClient, CkError, Command, ModuleCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Table {
    Health,
    Modules,
    Activity,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PlainOutput {
    Table(Table),
    PipedText,
    Exempt,
}

pub(super) fn plain_output(command: &Command) -> PlainOutput {
    match command {
        Command::Health => PlainOutput::Table(Table::Health),
        Command::Activity(_) => PlainOutput::Table(Table::Activity),
        Command::Module(module) => match module {
            ModuleCommand::List => PlainOutput::Table(Table::Modules),
            // Action acknowledgements, including swap, keep the piped ck form.
            ModuleCommand::Restart { .. }
            | ModuleCommand::Stop { .. }
            | ModuleCommand::Start { .. }
            | ModuleCommand::Swap { .. } => PlainOutput::Exempt,
            ModuleCommand::Status { .. }
            | ModuleCommand::Logs { .. }
            | ModuleCommand::StderrTail { .. }
            | ModuleCommand::Terminals { .. }
            | ModuleCommand::Rescan { .. }
            | ModuleCommand::ReleaseReserved { .. } => PlainOutput::PipedText,
        },
        Command::Setup(_)
        | Command::Upgrade { .. }
        | Command::Routes { .. }
        | Command::Catalog { .. }
        | Command::Provenance { .. }
        | Command::HealthDetail { .. }
        | Command::Daemon
        | Command::DaemonTriage
        | Command::MachineShow
        | Command::MachineAdopt { .. }
        | Command::Quota { .. }
        | Command::FleetLint { .. }
        | Command::Help(_) => PlainOutput::PipedText,
        // Probes print the same bytes under ckni as under ck. The dashboard
        // is refused; external ck-<domain> commands use their own output.
        Command::BuildShape
        | Command::CliFaceReservedNames
        | Command::Dashboard
        | Command::External { .. } => PlainOutput::Exempt,
    }
}

pub(super) async fn run_table(
    table: Table,
    client: &mut CkClient,
    verbose: bool,
) -> Result<(), CkError> {
    match table {
        Table::Health => {
            let value = super::supervisor_health(client).await?;
            let (headers, rows) = health_table(super::modules_array(&value), verbose);
            super::print_table(&headers, rows);
        }
        Table::Modules => {
            let roster = super::supervisor_list(client).await?;
            let spawns = client
                .rpc_value(ClientControlRequest::SupervisorSpawnSnapshot {})
                .await?;
            let provenance = client
                .rpc_value(ClientControlRequest::SupervisorProvenance { module_id: None })
                .await?;
            let now_ms = super::fixed_test_now_ms().unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            });
            let (headers, rows) = module_table(
                super::modules_array(&roster),
                &spawns,
                &provenance,
                now_ms,
                verbose,
            );
            super::print_table(&headers, rows);
        }
        Table::Activity => unreachable!("activity owns its query and optional stream"),
    }
    Ok(())
}

type Rows = Vec<Vec<String>>;

fn health_table(modules: &[Value], verbose: bool) -> (Vec<&'static str>, Rows) {
    let mut headers = vec!["MODULE", "HEALTH", "SUMMARY"];
    if verbose {
        headers.push("DETAILS");
    }
    let rows = modules
        .iter()
        .map(|module| {
            let mut row = vec![
                field(module, "module_id"),
                cell(&super::human_health_status(&field(module, "status"))),
                cell(&super::health_operator_detail(module).unwrap_or_default()),
            ];
            if verbose {
                row.push(json_cell(module));
            }
            row
        })
        .collect();
    (headers, rows)
}

fn module_table(
    modules: &[Value],
    spawns: &Value,
    provenance: &Value,
    now_ms: u64,
    verbose: bool,
) -> (Vec<&'static str>, Rows) {
    let mut headers = vec![
        "MODULE", "STATUS", "HEALTH", "PID", "UP (s)", "RESTARTS", "BUILD", "PENDING",
    ];
    if verbose {
        headers.push("DETAILS");
    }
    let rows = modules
        .iter()
        .map(|module| {
            let id = module
                .get("module_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let spawn = spawns
                .get("live")
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter()
                        .find(|row| row.get("module_id").and_then(Value::as_str) == Some(id))
                })
                .unwrap_or(&Value::Null);
            let provenance = super::find_module(provenance, id).unwrap_or(&Value::Null);
            let build = provenance
                .get("module_declared")
                .filter(|declared| {
                    declared.get("status").and_then(Value::as_str) == Some("reported")
                })
                .and_then(|declared| declared.pointer("/build/build_git_sha"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let build = if verbose {
                build.to_string()
            } else {
                build.chars().take(8).collect()
            };
            let pending = super::reload_list_marker(module);
            let pending = if pending.starts_with("pending") {
                "restart"
            } else if pending.starts_with("nothing") {
                "-"
            } else {
                "unknown"
            };
            let mut row = vec![
                cell(id),
                cell(&super::module_status_text(module)),
                cell(&super::human_health_status(&field(module, "health"))),
                field(spawn, "pid"),
                spawn
                    .get("spawned_at_ms")
                    .and_then(Value::as_u64)
                    .map(|at| (now_ms.saturating_sub(at) / 1000).to_string())
                    .unwrap_or_else(|| "-".into()),
                field(module, "lifetime_restarts"),
                cell(&build),
                pending.into(),
            ];
            if verbose {
                row.push(json_cell(module));
            }
            row
        })
        .collect();
    (headers, rows)
}

fn field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => cell(text),
        Some(Value::Null) | None => "-".into(),
        Some(other) => cell(&other.to_string()),
    }
}

pub(super) fn json_cell(value: &Value) -> String {
    if value.is_null() || value.as_object().is_some_and(|map| map.is_empty()) {
        "-".into()
    } else {
        cell(&value.to_string())
    }
}

pub(super) fn cell(text: &str) -> String {
    if text.is_empty() {
        "-".into()
    } else {
        super::activity::cell(&text.replace(" · ", "; ").replace('→', "to"))
    }
}

pub(super) fn activity_headers(verbose: bool) -> Vec<&'static str> {
    let mut headers = vec!["TIME (UTC)", "SUBJECT", "EVENT", "BY"];
    if verbose {
        headers.extend(["KIND", "SEQ", "DETAILS"]);
    }
    headers
}

pub(super) fn color_enabled(ckni: bool, stdout_terminal: bool, no_color: bool) -> bool {
    !ckni && stdout_terminal && !no_color
}

pub(super) fn exit_code(error: &CkError) -> i32 {
    match error {
        CkError::Usage(_) | CkError::FleetLintConfig(_) => 2,
        CkError::Discovery { .. } | CkError::Connection { .. } => 3,
        CkError::WithFooter { error, .. } => exit_code(error),
        CkError::FleetLintExit { exit_code }
        | CkError::TriageExit { exit_code }
        | CkError::RenderedExit { exit_code } => i32::from(*exit_code != 0),
        CkError::Rejected { .. }
        | CkError::Message(_)
        | CkError::Json(_)
        | CkError::UpdateCheck(_)
        | CkError::ModuleNotCommand { .. }
        | CkError::ResponseTimeout { .. }
        | CkError::OutcomeUnknown { .. } => 1,
    }
}

pub(super) fn error_text(error: &CkError) -> Option<String> {
    let (code, message, next) = error_parts(error)?;
    Some(format!(
        "ckni: {}: {}\n{}",
        super::activity::cell(code),
        super::activity::cell(&message),
        super::activity::cell(&next)
    ))
}

fn error_parts(error: &CkError) -> Option<(&str, String, String)> {
    let (code, next) = match error {
        CkError::Usage(_) | CkError::FleetLintConfig(_) => {
            ("usage", "Run ckni --help for the commands.".into())
        }
        CkError::Discovery { .. } | CkError::Connection { .. } => (
            "daemon_unreachable",
            "Check the connection file, then run ckni daemon --subc <connection-file>.".into(),
        ),
        CkError::ResponseTimeout { .. } => (
            "timeout",
            "Run ckni daemon to check whether the daemon is reachable.".into(),
        ),
        CkError::OutcomeUnknown { verify_command, .. } => {
            ("outcome_unknown", format!("verify with: {verify_command}"))
        }
        CkError::Rejected { code, .. } => (
            code.as_deref().unwrap_or("failed"),
            "Check the refusal above before retrying the command.".into(),
        ),
        CkError::WithFooter { error, footer } => {
            let (code, message, _) = error_parts(error)?;
            return Some((code, message, footer.clone()));
        }
        CkError::Message(_)
        | CkError::Json(_)
        | CkError::UpdateCheck(_)
        | CkError::ModuleNotCommand { .. } => (
            "failed",
            "Run ckni --help to check the command and its arguments.".into(),
        ),
        CkError::FleetLintExit { .. }
        | CkError::TriageExit { .. }
        | CkError::RenderedExit { .. } => return None,
    };
    let message = match error {
        CkError::Rejected {
            message,
            code: Some(code),
        } => message
            .strip_prefix(&format!("{code} — "))
            .unwrap_or(message)
            .to_string(),
        CkError::OutcomeUnknown { .. } => error.to_string().lines().next().unwrap_or("").into(),
        _ => error.to_string(),
    };
    Some((code, message, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{ffi::OsString, time::Duration};

    #[test]
    fn ckni_health_table_has_summary_and_verbose_only_diagnostics_without_trimming() {
        let entries = (0..205)
            .map(|i| {
                json!({
                    "module_id": format!("module-{i}"), "status": "ok", "detail": "working · ready",
                    "metrics": {"queue": 42}, "consecutive_failures": 3,
                })
            })
            .collect::<Vec<_>>();
        let (headers, rows) = health_table(&entries, false);
        assert_eq!(headers, ["MODULE", "HEALTH", "SUMMARY"]);
        assert_eq!(rows.len(), 205);
        assert_eq!(rows[204], ["module-204", "ok", "working; ready"]);
        assert!(rows
            .iter()
            .all(|row| row.len() == 3 && !row.join(" ").contains("queue")));
        let (headers, rows) = health_table(&entries, true);
        assert_eq!(headers.last(), Some(&"DETAILS"));
        assert!(rows[204][3].contains("\"queue\":42"));
        let (_, rows) = health_table(&[json!({"module_id":"quiet", "status":"ok"})], false);
        assert_eq!(rows[0], ["quiet", "ok", "-"]);
        assert!(health_table(&[], false).1.is_empty());
    }

    #[test]
    fn ckni_module_table_joins_by_id_and_uses_fixed_seconds_and_absence_marks() {
        let modules = vec![
            json!({
                "module_id": "alpha", "state": "running", "health": "ok", "enabled":true,
                "lifetime_restarts": 2, "restart_count": 1,
                "pending_reload": {"path":{"status":"match"}, "image":{"status":"match"}, "spec":{"status":"match"}},
            }),
            json!({"module_id":"missing", "state":"stopped", "health":"unknown", "enabled":false}),
        ];
        let spawns = json!({"live":[{"module_id":"other","pid":99,"spawned_at_ms":1}, {"module_id":"alpha", "pid":123,"spawned_at_ms":1000}]});
        let provenance = json!({"modules":[{"module_id":"other"}, {"module_id":"alpha", "module_declared":{"status":"reported","build":{"build_git_sha":"123456789abcdef"}}}]});
        let (headers, rows) = module_table(&modules, &spawns, &provenance, 91_999, false);
        assert_eq!(
            headers,
            ["MODULE", "STATUS", "HEALTH", "PID", "UP (s)", "RESTARTS", "BUILD", "PENDING"]
        );
        assert_eq!(
            rows[0],
            ["alpha", "running", "ok", "123", "90", "2", "12345678", "-"]
        );
        assert_eq!(
            rows[1],
            ["missing", "stopped", "unknown", "-", "-", "-", "-", "unknown"]
        );
        let (_, verbose) = module_table(&modules, &spawns, &provenance, 91_999, true);
        assert_eq!(verbose[0][6], "123456789abcdef");
        assert!(verbose[0][8].contains("restart_count"));
        let many = vec![modules[0].clone(); 205];
        assert_eq!(
            module_table(&many, &spawns, &provenance, 91_999, false)
                .1
                .len(),
            205
        );
        let mut pending = modules[0].clone();
        pending["pending_reload"]["spec"]["status"] = json!("mismatch");
        assert_eq!(
            module_table(&[pending], &spawns, &provenance, 91_999, false).1[0][7],
            "restart"
        );
    }

    #[test]
    fn ckni_cells_escape_ansi_and_newlines_without_cutting_long_values() {
        assert_eq!(cell("\x1b[31mred\nrow"), "\\u{1b}[31mred\\nrow");
        assert_eq!(cell(&"long".repeat(500)), "long".repeat(500));
        assert_eq!(cell(""), "-");
    }

    #[test]
    fn ckni_piped_text_never_enables_color_even_in_a_terminal() {
        for terminal in [false, true] {
            for no_color in [false, true] {
                assert!(!color_enabled(true, terminal, no_color));
                assert_eq!(
                    color_enabled(false, terminal, no_color),
                    terminal && !no_color
                );
            }
        }
        assert_eq!(error_text(&CkError::Message("\x1b[31mfailed\nnext".into())).unwrap(), "ckni: failed: \\u{1b}[31mfailed\\nnext\nRun ckni --help to check the command and its arguments.");
    }

    #[test]
    fn ckni_error_map_covers_every_variant_and_preserves_the_inner_footer_code() {
        let cases = vec![
            (CkError::Usage("bad args".into()), "usage", 2),
            (CkError::FleetLintConfig("bad config".into()), "usage", 2),
            (
                CkError::Discovery { tried: vec![] },
                "daemon_unreachable",
                3,
            ),
            (
                CkError::Connection {
                    path: "missing".into(),
                    source: "refused".into(),
                },
                "daemon_unreachable",
                3,
            ),
            (
                CkError::ResponseTimeout {
                    timeout: Duration::from_secs(10),
                },
                "timeout",
                1,
            ),
            (
                CkError::OutcomeUnknown {
                    operation: "restart".into(),
                    verify_command: "ck module status alpha".into(),
                    timeout: Duration::from_secs(10),
                },
                "outcome_unknown",
                1,
            ),
            (CkError::rejected("no code".into()), "failed", 1),
            (
                CkError::rejected_frame(br#"{"code":"fed_not_authorized","message":"denied"}"#),
                "fed_not_authorized",
                1,
            ),
            (CkError::Message("failed".into()), "failed", 1),
            (
                CkError::Json(serde_json::from_str::<Value>("{").unwrap_err()),
                "failed",
                1,
            ),
            (
                CkError::UpdateCheck(super::super::setup::UpdateCheckError::CacheWrite(
                    "denied".into(),
                )),
                "failed",
                1,
            ),
            (
                CkError::ModuleNotCommand {
                    command: "alpha".into(),
                    module_id: "alpha".into(),
                },
                "failed",
                1,
            ),
        ];
        for (error, code, exit) in cases {
            assert_eq!(exit_code(&error), exit, "{error:?}");
            let text = error_text(&error).unwrap();
            assert!(text.starts_with(&format!("ckni: {code}: ")), "{text}");
            assert!(text.lines().count() >= 2, "{text}");
            let wrapped = CkError::WithFooter {
                error: Box::new(error),
                footer: "check this next".into(),
            };
            assert_eq!(exit_code(&wrapped), exit);
            assert!(error_text(&wrapped)
                .unwrap()
                .starts_with(&format!("ckni: {code}: ")));
            assert!(error_text(&wrapped).unwrap().ends_with("\ncheck this next"));
        }
        for error in [
            CkError::FleetLintExit { exit_code: 2 },
            CkError::TriageExit { exit_code: 3 },
            CkError::RenderedExit { exit_code: 7 },
        ] {
            assert_eq!(exit_code(&error), 1);
            assert_eq!(error_text(&error), None);
        }
        assert_eq!(exit_code(&CkError::TriageExit { exit_code: 0 }), 0);
        let refusal =
            CkError::rejected_frame(br#"{"code":"fed_not_authorized","message":"denied"}"#);
        assert_eq!(refusal.to_string(), "fed_not_authorized — denied");
        assert_eq!(error_text(&refusal).unwrap(), "ckni: fed_not_authorized: denied\nCheck the refusal above before retrying the command.");
        assert_eq!(CkError::rejected_frame(b"not JSON").to_string(), "not JSON");
    }

    #[test]
    fn ckni_plain_command_walk_classifies_exactly_three_tables_and_the_piped_text_list() {
        let groups: &[(PlainOutput, &[&str])] = &[
            (PlainOutput::Table(Table::Health), &["health"]),
            (PlainOutput::Table(Table::Modules), &["module list"]),
            (PlainOutput::Table(Table::Activity), &["activity"]),
            (
                PlainOutput::PipedText,
                &[
                    "setup --dry-run",
                    "upgrade --check",
                    "module status alpha",
                    "module logs",
                    "module stderr alpha",
                    "module terminals alpha",
                    "module rescan --dry-run",
                    "module release alpha",
                    "routes",
                    "catalog",
                    "provenance alpha",
                    "health alpha",
                    "daemon",
                    "daemon triage",
                    "machine show",
                    "machine adopt example --force",
                    "quota",
                    "daemon lint",
                    "--help",
                ],
            ),
            (
                PlainOutput::Exempt,
                &[
                    "--ck-build-shape",
                    "--ck-cli-face-reserved-names",
                    "",
                    "external-face arg",
                    "module restart alpha",
                    "module restart alpha --swap",
                    "module stop alpha",
                    "module start alpha",
                ],
            ),
        ];
        for (expected, commands) in groups {
            for command in *commands {
                let args = std::iter::once(OsString::from("ckni"))
                    .chain(command.split_whitespace().map(OsString::from));
                let parsed = super::super::parse_args(args)
                    .unwrap_or_else(|error| panic!("{command}: {error}"));
                assert_eq!(&plain_output(&parsed.command), expected, "{command}");
            }
        }
    }
}
