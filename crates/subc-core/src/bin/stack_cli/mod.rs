use std::{ffi::OsString, path::PathBuf, time::Duration};

use subc_core::{
    stack::{
        Stack, StackResult, WindowCheck, START_HELP, START_TIMEOUT, STATUS_TIMEOUT, STOP_HELP,
        STOP_TIMEOUT,
    },
    window::Windows,
};

use super::{CkError, Command, JsonOutput};

pub(super) const HELP: &str = "ck stack - manage the stack

usage:
  ck stack stop [--timeout <secs>] [--json]    stop and wait for the stack (default: 70 s)
  ck stack start [--timeout <secs>] [--json]   start and wait for health (default: 120 s)
  ck stack status [--timeout <secs>] [--json]  report status, even offline (default: 2 s)";

pub(super) enum Action {
    Stop(Duration),
    Start(Duration),
    Status(Duration),
}

pub(super) fn parse(tail: &[OsString]) -> Result<Command, CkError> {
    let Some((verb, flags)) = tail.split_first() else {
        return Ok(Command::Help(HELP.into()));
    };
    let (action, default, detail): (fn(Duration) -> Action, _, _) = match verb.to_str() {
        Some("stop") => (Action::Stop, STOP_TIMEOUT, Some(STOP_HELP)),
        Some("start") => (Action::Start, START_TIMEOUT, Some(START_HELP)),
        Some("status") => (Action::Status, STATUS_TIMEOUT, None),
        Some("help" | "-h" | "--help") => return Ok(Command::Help(HELP.into())),
        _ => {
            return Err(CkError::Usage(format!(
                "unknown stack verb {verb:?}\n\n{HELP}"
            )))
        }
    };
    let timeout = match flags {
        [] => default,
        [flag, value] if flag == "--timeout" => value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .ok_or_else(|| CkError::Usage("--timeout requires a whole number of seconds".into()))?,
        [flag] if flag == "-h" || flag == "--help" => {
            return Ok(Command::Help(match detail {
                Some(detail) => format!("{HELP}\n\n{detail}"),
                None => HELP.into(),
            }));
        }
        _ => return Err(CkError::Usage(HELP.into())),
    };
    Ok(Command::Stack(action(timeout)))
}

pub(super) async fn run(
    action: Action,
    connection: Option<PathBuf>,
    output: JsonOutput,
) -> Result<(), CkError> {
    tokio::task::spawn_blocking(move || {
        let connection = connection.as_deref();
        let stack = Stack::current(connection);
        let (result, stopping) = match action {
            Action::Status(timeout) => {
                let status = stack
                    .and_then(|stack| stack.status(timeout))
                    .map_err(CkError::Usage)?;
                return output.completion(&status, 0, || status.human());
            }
            Action::Stop(timeout) => (
                stack.map_or_else(StackResult::refused, |stack| stack.stop(timeout)),
                true,
            ),
            Action::Start(timeout) => (
                stack.map_or_else(StackResult::refused, |stack| {
                    stack.start(timeout, || match Windows::discover(connection) {
                        Ok(windows) => windows.stack_check(None),
                        Err(error) => WindowCheck::Unreadable {
                            id: "windows".into(),
                            error: error.message,
                        },
                    })
                }),
                false,
            ),
        };
        output.completion(&result, result.exit.into(), || result.human(stopping))
    })
    .await
    .map_err(|error| CkError::Message(format!("stack command did not finish: {error}")))?
}
