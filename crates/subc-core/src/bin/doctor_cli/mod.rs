use std::{ffi::OsString, io::Write, sync::Arc};

use serde::Serialize;
use subc_core::doctor::{
    self, DoctorOptions, DoctorReport, DoctorRun, FixSkip, Layer, LogFallback, LogFallbackRead,
    LogTail, LogTailRecord, ReportFile, Severity,
};

use crate::{log_tail, Command};

mod report_file;
use report_file::ReportTarget;

// Keep triage in its own source file without widening the helpers' visibility.
include!("triage.rs");

pub(super) const HELP: &str = "ck doctor — diagnose the stack, even when the daemon is down

usage:
  ck doctor [--json] [--report <file>] [--fix] [--subc <connection-file>]

  --json           print one JSON document; for a completed run, stderr is used only if the report cannot be written
  --report <file>  save a private, unsanitized report with bounded log tails
  --fix            attempt an offered window revert with system approval
  --subc <file>    check a specific daemon; cannot be used with --fix
  --force          not available";

#[derive(Debug, Default)]
pub(super) struct Options {
    report: Option<PathBuf>,
    fix: bool,
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

pub(super) fn run(
    options: Options,
    explicit_connection: Option<PathBuf>,
    output: JsonOutput,
) -> Result<(), CkError> {
    let connection_override = explicit_connection.or_else(|| {
        env::var_os("SUBC_CONNECTION_FILE")
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
    let result = doctor::run(
        DoctorOptions {
            connection_override,
            fix: options.fix,
        },
        Arc::new(LocalLogs),
    );
    let exit_code = complete(
        result,
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

fn write_full_report(file: &mut fs::File, report: &FullReport<'_>) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *file, report)?;
    writeln!(file)
}

fn complete(
    mut result: DoctorRun,
    target: Option<ReportTarget>,
    output: JsonOutput,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    write_report: impl FnOnce(&mut fs::File, &FullReport<'_>) -> io::Result<()>,
) -> Result<u8, CkError> {
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
    if output.enabled {
        let document = output.value(&serde_json::to_value(&result.document)?);
        serde_json::to_writer_pretty(&mut *stdout, &document)?;
        writeln!(stdout).map_err(stdout_error)?;
    } else {
        write_human(stdout, &result.document, incomplete.as_deref()).map_err(stdout_error)?;
    }
    stdout.flush().map_err(stdout_error)?;
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
    let mut current_layer = None;
    for finding in &report.findings {
        if current_layer != Some(finding.layer) {
            current_layer = Some(finding.layer);
            let name = match finding.layer {
                Layer::Install => "install",
                Layer::Service => "service",
                Layer::Daemon => "daemon",
                Layer::Disk => "disk",
                Layer::Window => "window",
                Layer::Module => "module",
            };
            writeln!(out, "{name}:")?;
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
        if let Some(fields) = evidence.as_object_mut() {
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
                Some(FixSkip::NothingToFix) => writeln!(out, "Nothing to fix.")?,
                Some(FixSkip::ManagerObservationOverdue) => {
                    writeln!(
                        out,
                        "Fix skipped: the service manager observation is still running."
                    )?;
                }
                None => (),
            }
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
            let error = crate::run(argv).await.unwrap_err();
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
