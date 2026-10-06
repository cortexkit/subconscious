#[cfg(target_os = "macos")]
use std::process::Command;
use std::{
    io::Write,
    process::{Child, Output, Stdio},
    time::{Duration, Instant},
};
#[cfg(target_os = "macos")]
use subc_os::privacy_identity::ConfirmationError;
use subc_os::privacy_identity::{probe, DisclaimedCommand};

const FIXTURE: &str = env!("CARGO_BIN_EXE_subc-os-privacy-fixture");

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn builder() -> DisclaimedCommand {
    let mut builder = DisclaimedCommand::new(FIXTURE, FIXTURE);
    builder
        .env_remove("SUBC_TEST_PRIVACY_MISSING_SYMBOL")
        .env_remove("SUBC_TEST_PRIVACY_EXEC_DELAY_MS")
        .env_remove("SUBC_TEST_PRIVACY_CLOSE_ACK_WITHOUT_EXEC")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    builder
}

// Reap even when an assertion fails, especially when the timeout test deliberately
// leaves a trampoline waiting rather than executing its target.
struct ReapedChild(Option<Child>);

impl ReapedChild {
    fn output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn(builder: DisclaimedCommand) -> (ReapedChild, subc_os::privacy_identity::ExecConfirmation) {
    let (mut command, confirmation) = builder.into_command().unwrap();
    let child = command.spawn().unwrap();
    drop(command);
    (ReapedChild(Some(child)), confirmation)
}

#[test]
fn cooperating_trampoline_passes_startup_probe() {
    probe(FIXTURE, deadline()).unwrap();
}

#[test]
fn stdio_arguments_environment_and_working_directory_reach_child() {
    let cwd = std::env::temp_dir().canonicalize().unwrap();
    let mut builder = builder();
    builder
        .arg("io")
        .arg("first argument")
        .args(["--second", "third"])
        .env("SUBC_COMMAND_VALUE", "value with spaces")
        .current_dir(&cwd)
        .stdin(Stdio::piped());
    let (mut child, confirmation) = spawn(builder);
    child
        .0
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .unwrap()
        .write_all(b"input from parent")
        .unwrap();
    confirmation.confirm(deadline()).unwrap();
    let output = child.output();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "stdin=input from parent\nenv=value with spaces\ncwd={}\n",
            cwd.display()
        )
    );
    assert_eq!(output.stderr, b"child stderr\n");
}

#[test]
fn env_clear_and_env_remove_leave_only_requested_environment() {
    assert!(
        std::env::vars_os().next().is_some(),
        "parent environment is not empty"
    );
    let mut builder = builder();
    builder
        .arg("environment")
        .env("SUBC_COMMAND_BEFORE_CLEAR", "discarded")
        .env_clear()
        .env("SUBC_COMMAND_VALUE", "kept")
        .env("SUBC_COMMAND_REMOVED", "discarded")
        .env_remove("SUBC_COMMAND_REMOVED");
    let (child, confirmation) = spawn(builder);
    confirmation.confirm(deadline()).unwrap();
    let output = child.output();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"SUBC_COMMAND_VALUE=kept\n");
}

#[test]
fn normal_exec_confirms_even_when_child_exits_with_reserved_status() {
    let mut builder = builder();
    builder.args(["exit", "120"]);
    let (child, confirmation) = spawn(builder);
    confirmation.confirm(deadline()).unwrap();
    assert_eq!(child.output().status.code(), Some(120));
}

#[cfg(target_os = "macos")]
#[test]
fn dropping_confirmation_before_spawn_preserves_owned_descriptor() {
    let mut builder = builder();
    builder.args(["exit", "0"]);
    let (mut command, confirmation) = builder.into_command().unwrap();
    drop(confirmation);
    let child = ReapedChild(Some(command.spawn().unwrap()));
    drop(command);
    assert!(child.output().status.success());
}

#[cfg(target_os = "macos")]
#[test]
fn child_is_its_own_responsible_process() {
    let mut builder = builder();
    builder.arg("responsibility");
    let (child, confirmation) = spawn(builder);
    let pid = child.0.as_ref().unwrap().id();
    confirmation.confirm(deadline()).unwrap();
    let output = child.output();
    assert!(output.status.success(), "{output:?}");
    let report: Vec<u32> = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(report.len(), 4);
    assert_eq!(report[0], pid, "SETEXEC keeps the child pid");
    assert_eq!(report[3], std::process::id());
    assert_eq!(report[1], pid, "child must be its own responsible process");
    assert_ne!(
        report[1], report[2],
        "child must not inherit parent responsibility"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn direct_spawn_control_inherits_parent_responsibility() {
    let output = Command::new(FIXTURE)
        .arg("responsibility")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Vec<u32> = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(report.len(), 4);
    assert_ne!(report[0], report[1]);
    assert_eq!(report[1], report[2]);
}

#[cfg(target_os = "macos")]
#[test]
fn confirmation_names_missing_symbol_refusal() {
    let mut builder = builder();
    builder
        .args(["exit", "0"])
        .env("SUBC_TEST_PRIVACY_MISSING_SYMBOL", "1");
    let (child, confirmation) = spawn(builder);
    let error = confirmation.confirm(deadline()).unwrap_err();
    let ConfirmationError::Refused(error) = error else {
        panic!("expected a named exec refusal, got {error}");
    };
    assert_eq!(error.exit_code(), 120);
    assert!(error
        .to_string()
        .contains("responsibility_spawnattrs_setdisclaim is unavailable"));
    assert_eq!(child.output().status.code(), Some(120));
}

#[cfg(target_os = "macos")]
#[test]
fn confirmation_timeout_is_an_error() {
    let mut builder = builder();
    builder
        .args(["exit", "0"])
        .env("SUBC_TEST_PRIVACY_EXEC_DELAY_MS", "30000");
    let (child, confirmation) = spawn(builder);
    let started = Instant::now();
    let result = confirmation.confirm(started + Duration::from_millis(30));
    drop(child);
    assert!(
        matches!(result, Err(ConfirmationError::TimedOut)),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "deadline did not bound waiting"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn startup_probe_rejects_noncooperating_binary() {
    // /usr/bin/true exits successfully but does not speak the trampoline protocol.
    assert!(matches!(
        probe("/usr/bin/true", deadline()),
        Err(ConfirmationError::Io(_))
    ));
    assert!(matches!(
        probe(FIXTURE, Instant::now()),
        Err(ConfirmationError::TimedOut)
    ));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn non_macos_ignores_trampoline_and_confirms_at_once() {
    let expired = Instant::now() - Duration::from_secs(1);
    probe("/nonexistent/trampoline", expired).unwrap();
    let (command, confirmation) = DisclaimedCommand::new("/nonexistent/trampoline", FIXTURE)
        .into_command()
        .unwrap();
    assert_eq!(command.get_program(), FIXTURE);
    confirmation.confirm(expired).unwrap();
}

#[cfg(feature = "tokio")]
#[test]
fn tokio_conversion_preserves_the_command_and_confirmation() {
    // No runtime is needed to inspect or exercise the underlying standard command.
    let mut builder = builder();
    builder.args(["exit", "0"]);
    let (mut command, confirmation) = builder.into_tokio_command().unwrap();
    let child = ReapedChild(Some(command.as_std_mut().spawn().unwrap()));
    drop(command);
    confirmation.confirm(deadline()).unwrap();
    assert!(child.output().status.success());
}
