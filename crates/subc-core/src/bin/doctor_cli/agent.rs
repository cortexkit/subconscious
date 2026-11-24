use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    io::{self, IsTerminal},
    path::Path,
    sync::Arc,
};

use subc_core::doctor::{self, rescue, DoctorOptions, DoctorReport, ReportFile, Severity};
use subc_daemon::daemon_config;

use super::{write_full_report, FullReport, LocalLogs, Options};
use crate::CkError;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Rescue,
    Forget,
}

#[derive(Clone, Copy)]
struct Invocation<'a> {
    explicit_connection: bool,
    connection_env: Option<&'a OsStr>,
    rescue_session: bool,
    ckni: bool,
    stdin_terminal: bool,
    stdout_terminal: bool,
}

fn mode(options: &Options, invocation: Invocation<'_>) -> Result<Mode, &'static str> {
    if options.forget && !options.agent {
        return Err("doctor --forget requires --agent");
    }
    if options.fix || options.report.is_some() {
        return Err("doctor --agent cannot be used with --fix or --report");
    }
    if invocation.rescue_session {
        return Err("doctor --agent cannot run inside a rescue session (CK_RESCUE_SESSION is set)");
    }
    if options.forget {
        return Ok(Mode::Forget);
    }
    if invocation.explicit_connection {
        return Err("doctor --agent cannot be used with --subc");
    }
    if invocation
        .connection_env
        .is_some_and(|path| !path.is_empty())
    {
        return Err("doctor --agent cannot be used with SUBC_CONNECTION_FILE");
    }
    if invocation.ckni {
        return Err("doctor --agent cannot run under ckni");
    }
    if !invocation.stdin_terminal {
        return Err("doctor --agent requires stdin to be a terminal");
    }
    if !invocation.stdout_terminal {
        return Err("doctor --agent requires stdout to be a terminal");
    }
    Ok(Mode::Rescue)
}

pub(super) fn run(options: &Options, explicit_connection: bool, ckni: bool) -> Result<(), CkError> {
    let connection_env = env::var_os("SUBC_CONNECTION_FILE");
    let invocation = Invocation {
        explicit_connection,
        connection_env: connection_env.as_deref(),
        rescue_session: env::var_os("CK_RESCUE_SESSION").is_some(),
        ckni,
        stdin_terminal: io::stdin().is_terminal(),
        stdout_terminal: io::stdout().is_terminal(),
    };
    match mode(options, invocation).map_err(|reason| CkError::Usage(reason.into()))? {
        Mode::Rescue => run_rescue(),
        Mode::Forget => {
            let login = rescue::forget().map_err(rescue_error)?;
            let status = if login.removed {
                "Deleted"
            } else {
                "Not found:"
            };
            println!("{status} {}", login.path.display());
            Ok(())
        }
    }
}

fn run_rescue() -> Result<(), CkError> {
    let prepared = rescue::prepare().map_err(rescue_error)?;
    let mut result = doctor::run(DoctorOptions::default(), Arc::new(LocalLogs));
    let report_path = prepared.session_dir().join("report.json");
    result.document.report = Some(ReportFile {
        path: report_path.clone(),
        sanitized: false,
    });
    let mut bytes = Vec::new();
    write_full_report(
        &mut bytes,
        &FullReport {
            document: &result.document,
            log_tails: &result.log_tails,
        },
    )
    .map_err(|error| CkError::Message(format!("could not serialize {report_path:?}: {error}")))?;
    prepared
        .write_session_file("report.json", &bytes)
        .map_err(rescue_error)?;

    let run_dir = prepared.daemon_run_dir();
    let data_home = run_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| CkError::Message(format!("could not find data home from {run_dir:?}")))?;
    let ck = env::current_exe()
        .map_err(|error| CkError::Message(format!("could not locate the running ck: {error}")))?;
    let mut prompt = render_prompt(
        prepared.install_root(),
        data_home,
        &daemon_config::default_config_path(),
        &run_dir.join("logs"),
        &ck,
    )?;
    append_summary(&mut prompt, &result.document);
    prepared
        .write_session_file("rescue-prompt.md", prompt.as_bytes())
        .map_err(rescue_error)?;
    println!("The rescue agent can read report.json and send its contents to the provider you signed in with.");
    #[cfg(windows)]
    println!("Pi's bash tool needs Git Bash or WSL on Windows.");
    prepared.launch().map_err(rescue_error)
}

fn rescue_error(error: rescue::RescueError) -> CkError {
    CkError::Message(error.to_string())
}

fn render_prompt(
    install_home: &Path,
    data_home: &Path,
    config_file: &Path,
    log_dir: &Path,
    ck: &Path,
) -> Result<String, CkError> {
    let path_text = |path: &Path| {
        path.to_str()
            .filter(|value| {
                !value.contains(['`', '\n', '\r', '\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}'])
            })
            .map(str::to_owned)
            .ok_or_else(|| CkError::Message(format!("cannot write rescue prompt: {path:?} is not UTF-8 or contains a line break or backtick")))
    };
    let ck = path_text(ck)?;
    #[cfg(windows)]
    let ck = ck.replace('\\', "/");
    let ck = format!("'{}'", ck.replace('\'', "'\\''"));
    let install_home = path_text(install_home)?;
    let data_home = path_text(data_home)?;
    let config_file = path_text(config_file)?;
    let log_dir = path_text(log_dir)?;

    // Fill each slot once, so braces in a path never become another slot.
    let mut parts = include_str!("rescue_prompt.md").split("{{");
    let mut prompt = String::from(parts.next().unwrap_or_default());
    for part in parts {
        let (slot, rest) = part.split_once("}}").expect("closed rescue prompt slot");
        prompt.push_str(match slot {
            "ck" => &ck,
            "install_home" => &install_home,
            "data_home" => &data_home,
            "config_file" => &config_file,
            "log_dir" => &log_dir,
            _ => unreachable!("unknown rescue prompt slot {slot}"),
        });
        prompt.push_str(rest);
    }
    Ok(prompt)
}

fn append_summary(prompt: &mut String, report: &DoctorReport) {
    let mut counts = [0; 3];
    let answered = if report.daemon_answered { "yes" } else { "no" };
    writeln!(
        prompt,
        "\n## Local diagnostic summary\n\nDaemon answered: {answered}"
    )
    .unwrap();
    for finding in &report.findings {
        let (index, severity) = match finding.severity {
            Severity::Error => (0, "error"),
            Severity::Warning => (1, "warning"),
            Severity::Info => (2, "info"),
        };
        counts[index] += 1;
        writeln!(prompt, "- {severity}: `{}`", finding.id).unwrap();
    }
    let [errors, warnings, info] = counts;
    writeln!(
        prompt,
        "\nFindings: {errors} error(s), {warnings} warning(s), {info} info."
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_refusals_are_ordered_and_forget_only_checks_combinations_and_recursion() {
        let mut invocation = Invocation {
            explicit_connection: true,
            connection_env: Some(OsStr::new("connection.json")),
            rescue_session: true,
            ckni: true,
            stdin_terminal: false,
            stdout_terminal: false,
        };
        let mut options = Options {
            forget: true,
            fix: true,
            report: Some("report.json".into()),
            ..Options::default()
        };
        let refusal = |options: &Options, invocation: Invocation<'_>, reason: &str| {
            assert_eq!(mode(options, invocation), Err(reason));
        };
        refusal(&options, invocation, "doctor --forget requires --agent");
        options.agent = true;
        let conflict = "doctor --agent cannot be used with --fix or --report";
        refusal(&options, invocation, conflict);
        options.fix = false;
        refusal(&options, invocation, conflict);
        options.fix = true;
        options.report = None;
        refusal(&options, invocation, conflict);
        options.fix = false;
        refusal(
            &options,
            invocation,
            "doctor --agent cannot run inside a rescue session (CK_RESCUE_SESSION is set)",
        );
        invocation.rescue_session = false;
        assert_eq!(mode(&options, invocation), Ok(Mode::Forget));
        options.forget = false;
        refusal(
            &options,
            invocation,
            "doctor --agent cannot be used with --subc",
        );
        invocation.explicit_connection = false;
        refusal(
            &options,
            invocation,
            "doctor --agent cannot be used with SUBC_CONNECTION_FILE",
        );
        invocation.connection_env = Some(OsStr::new(""));
        refusal(&options, invocation, "doctor --agent cannot run under ckni");
        invocation.ckni = false;
        refusal(
            &options,
            invocation,
            "doctor --agent requires stdin to be a terminal",
        );
        invocation.stdin_terminal = true;
        refusal(
            &options,
            invocation,
            "doctor --agent requires stdout to be a terminal",
        );
        invocation.stdout_terminal = true;
        assert_eq!(mode(&options, invocation), Ok(Mode::Rescue));
    }

    #[test]
    fn prompt_inserts_distinct_paths_without_replacing_text_inside_them() {
        let prompt = render_prompt(
            Path::new("/install/{{data_home}}"),
            Path::new("/roaming"),
            Path::new("/config/cortexkit/subc.jsonc"),
            Path::new("/logs"),
            Path::new("/ck's bin/ck"),
        )
        .unwrap();
        assert!(prompt.contains("`/install/{{data_home}}`"));
        assert!(prompt.contains("`/roaming`"));
        assert!(prompt.contains("`/config/cortexkit/subc.jsonc`"));
        assert!(prompt.contains("`/logs`"));
        assert!(prompt.contains(r"`'/ck'\''s bin/ck' doctor`"));
        for path in ["/bad\npath", "/bad\rpath", "/bad`path"] {
            let error = render_prompt(
                Path::new("/install"),
                Path::new("/data"),
                Path::new("/config"),
                Path::new(path),
                Path::new("/ck"),
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), 1);
            assert!(error.to_string().contains("line break or backtick"));
        }
    }
}
