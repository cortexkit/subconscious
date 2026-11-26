use std::{ffi::OsString, io::Write, sync::Arc};

use serde::Serialize;
use subc_core::doctor::{
    self, DoctorOptions, DoctorReport, DoctorRun, FixSkip, Interrupt, Layer, LogFallback,
    LogFallbackRead, LogTail, LogTailRecord, ModuleFix, ModuleFixDecision, ModuleFixOutcome,
    ModuleFixSkip, ModuleRestart, Programs, Prompter, ReportFile, Severity,
};

use super::{log_tail, response_budget, Command};

mod agent;
#[cfg(unix)]
mod interrupt;
mod issue;
mod report_file;
use report_file::ReportTarget;

// Keep triage in its own source file without widening the helpers' visibility.
include!("triage.rs");

pub(super) const HELP: &str = "ck doctor — diagnose the stack, even when the daemon is down

usage:
  ck doctor [--json] [--report <file>] [--fix] [--subc <connection-file>]
  ck doctor --issue [--report <file>]
  ck doctor --agent [--forget]

  --json           print one JSON document; for a completed run, stderr is used only if the report cannot be written
  --report <file>  save a private, unsanitized report with bounded log tails
  --fix            attempt an offered window revert or ask to apply module fixes
  --issue          save sanitized module reports and ask before filing publicly (local, human mode only)
  --agent          open the standalone Pi rescue agent for the local stack
  --forget         with --agent, remove only the local rescue login and exit
  --subc <file>    check a specific daemon; cannot be used with --fix or an interactive rescue
  --force          not available";

#[derive(Debug, Default)]
pub(super) struct Options {
    report: Option<PathBuf>,
    fix: bool,
    issue: bool,
    pub(super) agent: bool,
    forget: bool,
}

pub(super) fn parse(
    tail: Vec<OsString>,
    json: &mut bool,
    subc: &mut Option<PathBuf>,
) -> Result<Command, CkError> {
    if subc
        .as_deref()
        .is_some_and(|path| !valid_path_arg(path.as_os_str()))
    {
        return Err(CkError::Usage("doctor --subc requires a path".into()));
    }
    let mut options = Options::default();
    let mut help = false;
    let mut args = tail.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--json") => *json = true,
            Some("--subc") => *subc = Some(path_value(&mut args, "--subc")?),
            Some("--report") if options.report.is_none() => {
                options.report = Some(path_value(&mut args, "--report")?);
            }
            Some("--fix") if !options.fix => options.fix = true,
            Some("--issue") if !options.issue => options.issue = true,
            Some("--agent") if !options.agent => options.agent = true,
            Some("--forget") if !options.forget => options.forget = true,
            Some("--force") => {
                return Err(CkError::Usage("doctor --force is not available".into()))
            }
            Some("-h" | "--help" | "help") => help = true,
            _ => {
                return Err(CkError::Usage(format!(
                    "unknown or repeated doctor argument {arg:?}"
                )))
            }
        }
    }
    if help {
        Ok(Command::Help(HELP.into()))
    } else {
        Ok(Command::Doctor(options))
    }
}

fn path_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, CkError> {
    args.next()
        .filter(|value| valid_path_arg(value))
        .map(PathBuf::from)
        .ok_or_else(|| CkError::Usage(format!("doctor {flag} requires a path")))
}

fn valid_path_arg(value: &OsStr) -> bool {
    !value.is_empty() && !value.to_string_lossy().starts_with('-')
}

struct LocalLogs;

impl LogFallback for LocalLogs {
    fn tail(&self, module_id: &str) -> LogFallbackRead {
        let (tail, run_folder_warning) = log_tail::read_bounded_tail(module_id);
        LogFallbackRead {
            tail: tail.map_err(|error| error.to_string()).map(|tail| {
                tail.map(|tail| LogTail {
                    module_id: module_id.into(),
                    entries: tail.entries.into_iter().map(|entry| entry.line).collect(),
                    truncated: tail.truncated,
                })
            }),
            run_folder_warning,
        }
    }
}

#[derive(Default)]
struct Terminal {
    #[cfg(unix)]
    tty: Option<Tty>,
}

#[cfg(unix)]
struct Tty {
    reader: std::sync::Mutex<io::BufReader<fs::File>>,
    writer: fs::File,
}

impl Terminal {
    fn open(ckni: bool) -> Self {
        #[cfg(unix)]
        if !ckni && env::var_os("CK_RESCUE_SESSION").is_none() {
            if let Ok(tty) = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
            {
                if let Ok(reader) = tty.try_clone() {
                    return Self {
                        tty: Some(Tty {
                            reader: std::sync::Mutex::new(io::BufReader::new(reader)),
                            writer: tty,
                        }),
                    };
                }
            }
        }
        #[cfg(not(unix))]
        let _ = ckni;
        Self::default()
    }

    #[cfg(unix)]
    fn write(&self, text: &str) -> io::Result<()> {
        let tty = self
            .tty
            .as_ref()
            .ok_or_else(|| io::Error::other("no terminal"))?;
        (&tty.writer).write_all(text.as_bytes())
    }

    #[cfg(unix)]
    fn ask(&self, text: &str) -> Option<String> {
        use io::BufRead;

        self.write(text).ok()?;
        let mut line = String::new();
        let read = self
            .tty
            .as_ref()?
            .reader
            .lock()
            .ok()?
            .read_line(&mut line)
            .ok()?;
        (read > 0).then(|| line.trim().to_owned())
    }

    #[cfg(unix)]
    fn yes(&self, question: &str) -> bool {
        self.ask(question).is_some_and(|answer| {
            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
        })
    }
}

impl Prompter for Terminal {
    fn interactive(&self) -> bool {
        #[cfg(unix)]
        {
            self.tty.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn confirm(&self, module: &str, finding: &cortexkit_doctor::Finding) -> bool {
        #[cfg(unix)]
        {
            let fix = finding.fix();
            let Some(plan) = fix.plan() else { return false };
            let mut prompt = format!("\n{module}: {}\n", finding.summary());
            for step in plan.steps() {
                prompt.push_str(&format!("  - {step}\n"));
            }
            prompt.push_str(if fix.needs_stop() {
                "The module must be stopped before applying this fix.\n"
            } else {
                "No module stop is needed.\n"
            });
            prompt.push_str("Apply this fix? [y/N] ");
            self.yes(&prompt)
        }
        #[cfg(not(unix))]
        {
            let _ = (module, finding);
            false
        }
    }
}

// The test executable can substitute a local GitHub API and simulate failed
// signal registration. The shipped ck never reads those settings from its environment.
#[cfg(unix)]
pub(crate) fn test_settings() -> (Option<OsString>, bool) {
    (None, false)
}

pub(super) fn run(
    options: Options,
    explicit_connection: Option<PathBuf>,
    output: JsonOutput,
) -> Result<(), CkError> {
    let env_connection = env::var_os("SUBC_CONNECTION_FILE");
    if options.issue {
        let conflict = [
            (options.fix, "--fix"),
            (output.ckni, "ckni"),
            (output.enabled, "--json"),
            (options.agent, "--agent"),
            (options.forget, "--forget"),
            (explicit_connection.is_some(), "--subc"),
            (env_connection.is_some(), "SUBC_CONNECTION_FILE"),
        ];
        if let Some((_, flag)) = conflict.into_iter().find(|(present, _)| *present) {
            return Err(CkError::Usage(format!(
                "doctor --issue cannot be used with {flag}"
            )));
        }
    }
    if options.agent || options.forget {
        return agent::run(&options, explicit_connection.is_some(), output.ckni);
    }
    let connection_override = explicit_connection.or_else(|| {
        env_connection
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    if options.fix && connection_override.is_some() {
        return Err(CkError::Usage(
            "doctor --fix cannot be used with --subc or SUBC_CONNECTION_FILE".into(),
        ));
    }
    let report = options
        .report
        .as_deref()
        .map(ReportTarget::create)
        .transpose()?;
    let terminal = Arc::new(Terminal::open(output.ckni));
    let interrupt = Arc::new(Interrupt::default());
    #[cfg(unix)]
    let (github_api, fail_sigterm) = crate::doctor_cli::test_settings();
    #[cfg(unix)]
    interrupt::listen(Arc::clone(&interrupt), Arc::clone(&terminal), fail_sigterm);
    let logs = Arc::new(LocalLogs);
    let doctor::ProgramRun { run, checked } = doctor::run_with_programs(
        DoctorOptions {
            connection_override,
            fix: options.fix,
        },
        Programs {
            prompter: terminal.as_ref(),
            interrupt: &interrupt,
            operator_budget: response_budget::operator(),
        },
        logs.clone(),
    );
    let issues = options.issue.then(|| {
        issue::file(
            checked,
            &terminal,
            &interrupt,
            logs.as_ref(),
            &run.document,
            #[cfg(unix)]
            github_api,
        )
    });
    let exit_code = complete(
        Completion {
            run,
            issues,
            interrupt: Some(&interrupt),
        },
        report,
        output,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
        write_full_report,
    )?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(CkError::StackWindowExit {
            exit_code: i32::from(exit_code),
        })
    }
}

#[derive(Serialize)]
struct FullReport<'a> {
    #[serde(flatten)]
    document: &'a DoctorReport,
    log_tails: &'a [LogTailRecord],
}

fn write_full_report(file: &mut impl Write, report: &FullReport<'_>) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *file, report)?;
    writeln!(file)
}

struct Completion<'a> {
    run: DoctorRun,
    issues: Option<issue::Rows>,
    interrupt: Option<&'a Interrupt>,
}

impl From<DoctorRun> for Completion<'_> {
    fn from(run: DoctorRun) -> Self {
        Self {
            run,
            issues: None,
            interrupt: None,
        }
    }
}

fn complete<'a>(
    completion: impl Into<Completion<'a>>,
    target: Option<ReportTarget>,
    output: JsonOutput,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    write_report: impl FnOnce(&mut fs::File, &FullReport<'_>) -> io::Result<()>,
) -> Result<u8, CkError> {
    let Completion {
        run: mut result,
        issues,
        interrupt,
    } = completion.into();
    let mut incomplete = None;
    if let Some(mut target) = target {
        result.document.report = Some(ReportFile {
            path: target.path.clone(),
            sanitized: false,
        });
        let full = FullReport {
            document: &result.document,
            log_tails: &result.log_tails,
        };
        if let Err(error) = target.write(|file| write_report(file, &full)) {
            result.document.report = None;
            result.exit_code = 1;
            incomplete = Some(target.path.clone());
            let message = error.to_string().lines().collect::<Vec<_>>().join(" ");
            // Failure to print a diagnostic must not suppress the completion document.
            let _ = writeln!(
                stderr,
                "Could not write doctor report {:?}: {message}",
                target.path
            );
        }
    }
    let printed = (|| {
        if output.enabled {
            let document = output.value(&serde_json::to_value(&result.document)?);
            serde_json::to_writer_pretty(&mut *stdout, &document)?;
            writeln!(stdout).map_err(stdout_error)?;
        } else {
            write_human(stdout, &result.document, incomplete.as_deref()).map_err(stdout_error)?;
            if let Some(issues) = issues {
                if issues.failed() {
                    result.exit_code = 1;
                }
                issues.write(stdout).map_err(stdout_error)?;
            }
        }
        stdout.flush().map_err(stdout_error)
    })();
    if let Some(signal) = interrupt.and_then(Interrupt::signal) {
        return Ok((128 + signal) as u8);
    }
    printed?;
    Ok(result.exit_code)
}

fn stdout_error(error: io::Error) -> CkError {
    CkError::Message(format!("could not print doctor report: {error}"))
}

fn write_human(
    out: &mut impl Write,
    report: &DoctorReport,
    incomplete: Option<&Path>,
) -> io::Result<()> {
    if let Some(path) = &report.connection_override {
        writeln!(out, "Connection override: {}", path.display())?;
    }
    let mut current_heading = None;
    for finding in &report.findings {
        let program_owner = (finding.layer == Layer::ModuleProgram).then_some(&finding.owner);
        let heading = (finding.layer, program_owner);
        if current_heading != Some(heading) {
            current_heading = Some(heading);
            let name = match finding.layer {
                Layer::Install => "install",
                Layer::Service => "service",
                Layer::Daemon => "daemon",
                Layer::Disk => "disk",
                Layer::Window => "window",
                Layer::Module => "module",
                Layer::ModuleProgram => "module program",
            };
            if let Some(owner) = program_owner {
                writeln!(out, "{name} {owner}:")?;
            } else {
                writeln!(out, "{name}:")?;
            }
        }
        let severity = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        writeln!(
            out,
            "  {severity} {} [{}]: {}",
            finding.id, finding.owner, finding.summary
        )?;
        let mut evidence = finding.evidence.clone();
        if finding.layer == Layer::ModuleProgram {
            writeln!(out, "    Evidence: {evidence}")?;
            if let Some(fix) = &finding.fix {
                if matches!(
                    fix.class(),
                    cortexkit_doctor::FixClass::Safe | cortexkit_doctor::FixClass::Risky
                ) {
                    if let Some(first) = fix.plan().and_then(|plan| plan.steps().first()) {
                        writeln!(out, "    {first} (run `ck doctor --fix` to apply)")?;
                    }
                }
            }
        } else if let Some(fields) = evidence.as_object_mut() {
            let tail = fields.remove("tail");
            let tails = fields.remove("tails");
            if !fields.is_empty() {
                writeln!(out, "    Evidence: {evidence}")?;
            }
            for preview in tail
                .iter()
                .chain(tails.iter().filter_map(Value::as_array).flatten())
            {
                let module = preview["module"].as_str().unwrap_or(&finding.owner);
                for entry in preview["entries"].as_array().into_iter().flatten() {
                    if let Some(text) = entry["text"].as_str() {
                        writeln!(out, "    {module}: {text}")?;
                    }
                }
            }
        }
    }
    if let Some(fix) = &report.fix {
        if let Some(result) = &fix.result {
            writeln!(out, "{}", result.human())?;
        } else {
            match fix.skipped {
                Some(FixSkip::NothingToFix) if fix.module_fixes.is_empty() => {
                    writeln!(out, "Nothing to fix.")?
                }
                Some(FixSkip::ManagerObservationOverdue) => {
                    writeln!(
                        out,
                        "Fix skipped: the service manager observation is still running."
                    )?;
                }
                Some(FixSkip::Interrupted) => writeln!(out, "Fix skipped: interrupted.")?,
                Some(FixSkip::NothingToFix) | None => (),
            }
        }
        for module in &fix.module_fixes {
            write_module_fix(out, module)?;
        }
        if fix
            .module_fixes
            .iter()
            .any(|row| row.skip_reason == Some(ModuleFixSkip::RevertFirst))
        {
            writeln!(out, "Run `ck doctor --fix` again to apply module fixes.")?;
        }
    }
    if let Some(saved) = &report.report {
        writeln!(out, "Saved unsanitized report to {}", saved.path.display())?;
    } else if let Some(path) = incomplete {
        writeln!(out, "Report at {} is incomplete.", path.display())?;
    }
    if report.connection_override.is_some() {
        writeln!(out, "Revert offer withheld: rerun without --subc or SUBC_CONNECTION_FILE to check the local stack.")?;
    }
    writeln!(
        out,
        "Use --report <file> to save a private, unsanitized report with log tails."
    )?;
    if let Some(offer) = &report.offer {
        writeln!(out, "{}", offer.command)?;
    }
    Ok(())
}

fn write_module_fix(out: &mut impl Write, fix: &ModuleFix) -> io::Result<()> {
    let decision = match fix.decision {
        ModuleFixDecision::Applied => "applied",
        ModuleFixDecision::Declined => "declined",
        ModuleFixDecision::Skipped => "skipped",
    };
    write!(
        out,
        "Module fix {} [{}]: {decision}",
        fix.finding, fix.module
    )?;
    if let Some(reason) = fix.skip_reason {
        let reason = match reason {
            ModuleFixSkip::NoTerminal => "no_terminal",
            ModuleFixSkip::RevertFirst => "revert_first",
            ModuleFixSkip::ManagerObservationOverdue => "manager_observation_overdue",
            ModuleFixSkip::StopFailed => "stop_failed",
            ModuleFixSkip::ProgramChanged => "program_changed",
            ModuleFixSkip::Interrupted => "interrupted",
            ModuleFixSkip::SignalSetupFailed => "signal_setup_failed",
        };
        write!(out, " {reason}")?;
    }
    if let Some(result) = &fix.result {
        let outcome = match result.outcome {
            ModuleFixOutcome::Fixed => "fixed",
            ModuleFixOutcome::Refused => "refused",
            ModuleFixOutcome::Failed => "failed",
            ModuleFixOutcome::NoResult => "no_result",
        };
        write!(out, "; {outcome}")?;
        if let Some(code) = &result.code {
            write!(out, " {code}")?;
            if result.outcome == ModuleFixOutcome::Refused && code == "plan_changed" {
                write!(out, " (run `ck doctor` again)")?;
            }
        }
    }
    if let Some(restart) = fix.restart {
        let restart = match restart {
            ModuleRestart::Enabled => "enabled",
            ModuleRestart::Failed => "failed",
        };
        write!(out, "; restart: {restart}")?;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use cortexkit_test_support::ScratchDir;
    use subc_core::doctor::Offer;

    use super::*;

    pub(super) fn diagnostic(exit_code: u8) -> DoctorRun {
        DoctorRun {
            document: DoctorReport {
                schema: "ck.doctor.report/v1",
                ok: exit_code == 0,
                daemon_answered: false,
                connection_override: None,
                findings: Vec::new(),
                offer: None,
                report: None,
                fix: None,
                modules: None,
            },
            log_tails: Vec::new(),
            exit_code,
        }
    }

    #[tokio::test]
    async fn precheck_refusals_do_not_create_a_report() {
        let dir = ScratchDir::new("doctor-argument-refusals");
        let target = dir.join("report.json");
        for tail in [
            &["--unknown"][..],
            &["--force"],
            &["--fix", "--force"],
            &["--fix", "--subc", "override.json"],
            &["--subc", "--fix"],
            &["--report", "--json"],
        ] {
            let argv = ["ck", "doctor", "--report"]
                .into_iter()
                .map(OsString::from)
                .chain([target.clone().into_os_string()])
                .chain(tail.iter().map(OsString::from));
            let error = super::super::run(argv).await.unwrap_err();
            assert_eq!(error.exit_code(), 2, "{tail:?}");
            assert!(!target.exists(), "{tail:?}");
        }
    }

    #[test]
    fn diagnostic_result_with_exit_one_emits_one_json_document_and_no_stderr() {
        let output =
            JsonOutput::for_command(&Command::Doctor(Options::default()), true, true).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = complete(
            diagnostic(1),
            None,
            output,
            &mut stdout,
            &mut stderr,
            |_, _| {
                panic!("there is no report file to write");
            },
        )
        .unwrap();
        assert_eq!(code, 1);
        assert!(stderr.is_empty());
        let document: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(document["schema"], "ck.doctor.report/v1");
        assert_eq!(document["ok"], false);
        assert_eq!(document["report"], Value::Null);
        assert!(document.get("log_tails").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn latched_sigterm_overrides_a_report_write_failure() {
        let dir = ScratchDir::new("doctor-signal-write-error");
        let target = ReportTarget::create(&dir.join("report.json")).unwrap();
        let output =
            JsonOutput::for_command(&Command::Doctor(Options::default()), true, false).unwrap();
        let interrupt = Interrupt::default();
        let guard = interrupt
            .protect("a module fix", std::time::Instant::now())
            .unwrap();
        assert!(interrupt.on_signal(15).is_some());
        drop(guard);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = complete(
            Completion {
                run: diagnostic(0),
                issues: None,
                interrupt: Some(&interrupt),
            },
            Some(target),
            output,
            &mut stdout,
            &mut stderr,
            |_, _| Err(io::Error::other("simulated write failure")),
        )
        .unwrap();
        assert_eq!(code, 143);
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("simulated write failure"));
        let document: Value = serde_json::from_slice(&stdout).unwrap();
        assert!(document["report"].is_null());
    }

    #[test]
    fn human_output_ends_with_report_hint_then_revert() {
        let mut result = diagnostic(1);
        let command = "ck window revert 20261123-010203-abcd";
        result.document.offer = Some(Offer {
            window_id: "20261123-010203-abcd".into(),
            command: command.into(),
        });
        let mut stdout = Vec::new();
        write_human(&mut stdout, &result.document, None).unwrap();
        let text = String::from_utf8(stdout).unwrap();
        let mut last_lines = text.lines().rev();
        assert_eq!(last_lines.next(), Some(command));
        assert!(last_lines.next().unwrap().contains("--report <file>"));
    }
}
