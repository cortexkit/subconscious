use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use subc_core::window::{journal::Role, Close, RecordRequest, RevertResult, WindowError, Windows};

use super::{CkError, Command, JsonOutput};

pub(super) const HELP: &str = "ck window - keep or undo recorded placement changes

usage:
  ck window begin [--note <text>]
  ck window current
  ck window list [--json]
  ck window record <file|bundle|store|activation> --window <id> --owner-pid <pid> ...
  ck window done <index> --window <id> --owner-pid <pid>
  ck window keep [<id>] [--json]
  ck window abandon <id> [--json]
  ck window revert [<id>] [--json]

Run 'ck window record --help' for placement arguments.";

const RECORD_USAGE: &str =
    "usage: ck window record <kind> --window <id> --owner-pid <pid> <kind options>

  file --target <path> (--new <path> | --remove) [--module <id> --role binary|format-map]
  bundle --target <dir> --new <dir>
  store --store <path> --snapshot <path> --module <id>
  activation --target daemon|<module-id>";

pub(super) enum Action {
    Begin(Option<String>),
    Current,
    List,
    Record {
        id: String,
        owner: u32,
        request: RecordRequest,
    },
    Done {
        id: String,
        owner: u32,
        index: usize,
    },
    Keep(Option<String>),
    Abandon(String),
    Revert(Option<String>),
}

pub(super) fn parse(tail: &[OsString]) -> Result<Command, CkError> {
    let Some((verb, args)) = tail.split_first() else {
        return Ok(Command::Help(HELP.into()));
    };
    if matches!(verb.to_str(), Some("help" | "-h" | "--help")) {
        return Ok(Command::Help(HELP.into()));
    }
    if matches!(args, [flag] if is_help(flag))
        || verb == "record" && matches!(args, [_, flag] if is_help(flag))
    {
        let help = if verb == "record" {
            format!("{RECORD_USAGE}\n\n{}", subc_core::window::RECORD_HELP)
        } else {
            HELP.into()
        };
        return Ok(Command::Help(help));
    }
    let action = match (verb.to_str(), args) {
        (Some("begin"), []) => Action::Begin(None),
        (Some("begin"), [flag, note]) if flag == "--note" => {
            Action::Begin(Some(text(note, "--note")?))
        }
        (Some("current"), []) => Action::Current,
        (Some("list"), []) => Action::List,
        (Some("keep"), []) => Action::Keep(None),
        (Some("keep"), [id]) if !is_flag(id) => Action::Keep(Some(text(id, "window id")?)),
        (Some("abandon"), [id]) if !is_flag(id) => Action::Abandon(text(id, "window id")?),
        (Some("revert"), []) => Action::Revert(None),
        (Some("revert"), [id]) if !is_flag(id) => Action::Revert(Some(text(id, "window id")?)),
        (Some("record"), [kind, flags @ ..]) => parse_record(kind, flags)?,
        (Some("done"), flags) => {
            let options = Options::parse(flags, &[], true)?;
            let (id, owner) = options.scope()?;
            let index = options
                .index
                .and_then(OsStr::to_str)
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| *index > 0)
                .ok_or_else(|| {
                    CkError::Usage("window done requires a positive step index".into())
                })?;
            Action::Done { id, owner, index }
        }
        _ => return Err(CkError::Usage(HELP.into())),
    };
    Ok(Command::Window(action))
}

fn is_help(arg: &OsStr) -> bool {
    arg == "--help" || arg == "-h"
}

fn is_flag(arg: &OsStr) -> bool {
    arg.as_encoded_bytes().starts_with(b"-")
}

fn text(arg: &OsStr, name: &str) -> Result<String, CkError> {
    arg.to_str()
        .map(str::to_owned)
        .ok_or_else(|| CkError::Usage(format!("{name} must be UTF-8")))
}

fn parse_record(kind: &OsStr, flags: &[OsString]) -> Result<Action, CkError> {
    let extra: &[&str] = match kind.to_str() {
        Some("file") => &["--target", "--new", "--remove", "--module", "--role"],
        Some("bundle") => &["--target", "--new"],
        Some("store") => &["--store", "--snapshot", "--module"],
        Some("activation") => &["--target"],
        _ => return Err(CkError::Usage(RECORD_USAGE.into())),
    };
    let options = Options::parse(flags, extra, false)?;
    let (id, owner) = options.scope()?;
    let request = match kind.to_str() {
        Some("file") => {
            let new = match (options.get("--new"), options.get("--remove").is_some()) {
                (Some(path), false) => Some(PathBuf::from(path)),
                (None, true) => None,
                _ => {
                    return Err(CkError::Usage(
                        "record file needs exactly one of --new or --remove".into(),
                    ))
                }
            };
            let module = options
                .get("--module")
                .map(|arg| text(arg, "--module"))
                .transpose()?;
            let role = match options.get("--role") {
                None => None,
                Some(arg) if arg == "binary" => Some(Role::Binary),
                Some(arg) if arg == "format-map" => Some(Role::FormatMap),
                Some(_) => {
                    return Err(CkError::Usage("--role must be binary or format-map".into()))
                }
            };
            RecordRequest::File {
                target: options.required("--target")?.into(),
                new,
                module,
                role,
            }
        }
        Some("bundle") => RecordRequest::Bundle {
            target: options.required("--target")?.into(),
            new: options.required("--new")?.into(),
        },
        Some("store") => RecordRequest::Store {
            store: options.required("--store")?.into(),
            snapshot: options.required("--snapshot")?.into(),
            module: text(options.required("--module")?, "--module")?,
        },
        Some("activation") => RecordRequest::Activation {
            target: text(options.required("--target")?, "--target")?,
        },
        _ => unreachable!("checked record kind"),
    };
    Ok(Action::Record { id, owner, request })
}

struct Options<'a> {
    values: BTreeMap<&'a str, &'a OsStr>,
    index: Option<&'a OsStr>,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [OsString], extra: &[&str], allow_index: bool) -> Result<Self, CkError> {
        let mut options = Self {
            values: BTreeMap::new(),
            index: None,
        };
        let mut args = args.iter().map(OsString::as_os_str);
        while let Some(arg) = args.next() {
            if !is_flag(arg) && allow_index && options.index.is_none() {
                options.index = Some(arg);
                continue;
            }
            let flag = arg
                .to_str()
                .filter(|flag| matches!(*flag, "--window" | "--owner-pid") || extra.contains(flag))
                .ok_or_else(|| CkError::Usage(format!("unexpected argument {arg:?}")))?;
            let value = if flag == "--remove" {
                OsStr::new("")
            } else {
                args.next()
                    .filter(|value| !is_flag(value))
                    .ok_or_else(|| CkError::Usage(format!("{flag} requires a value")))?
            };
            if options.values.insert(flag, value).is_some() {
                return Err(CkError::Usage(format!("{flag} may only be given once")));
            }
        }
        Ok(options)
    }

    fn get(&self, name: &str) -> Option<&'a OsStr> {
        self.values.get(name).copied()
    }

    fn required(&self, name: &str) -> Result<&'a OsStr, CkError> {
        self.get(name)
            .ok_or_else(|| CkError::Usage(format!("{name} is required")))
    }

    fn scope(&self) -> Result<(String, u32), CkError> {
        let id = text(self.required("--window")?, "--window")?;
        let owner = self
            .required("--owner-pid")?
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| CkError::Usage("--owner-pid must be a positive process id".into()))?;
        Ok((id, owner))
    }
}

pub(super) async fn run(
    action: Action,
    connection: Option<PathBuf>,
    output: JsonOutput,
) -> Result<(), CkError> {
    tokio::task::spawn_blocking(move || execute(action, connection, output))
        .await
        .map_err(|error| CkError::Message(format!("window command did not finish: {error}")))?
}

fn execute(action: Action, connection: Option<PathBuf>, output: JsonOutput) -> Result<(), CkError> {
    let windows = Windows::discover(connection.as_deref());
    match action {
        Action::Keep(id) => close(
            output,
            "Kept",
            windows.and_then(|windows| windows.keep(id.as_deref())),
        ),
        Action::Abandon(id) => close(
            output,
            "Abandoned",
            windows.and_then(|windows| windows.abandon(&id)),
        ),
        Action::Revert(id) => {
            let result = windows.map_or_else(
                |error| {
                    let mut result = RevertResult::default();
                    result.report.message = Some(error.message);
                    result
                },
                |windows| windows.revert(id.as_deref()),
            );
            output.completion(&result, result.exit(), || result.human())
        }
        Action::Begin(note) => {
            println!(
                "{}",
                windows
                    .and_then(|windows| windows.begin(note.as_deref()))
                    .map_err(window_error)?
            );
            Ok(())
        }
        Action::Current => {
            match windows
                .and_then(|windows| windows.current())
                .map_err(window_error)?
            {
                Some(id) => println!("{id}"),
                None => return Err(CkError::StackWindowExit { exit_code: 3 }),
            }
            Ok(())
        }
        Action::List => {
            let list = windows
                .and_then(|windows| windows.list())
                .map_err(window_error)?;
            output.completion(&list, 0, || list.human())
        }
        Action::Record { id, owner, request } => {
            println!(
                "{}",
                windows
                    .and_then(|windows| windows.record(&id, owner, request))
                    .map_err(window_error)?
            );
            Ok(())
        }
        Action::Done { id, owner, index } => windows
            .and_then(|windows| windows.done(&id, owner, index))
            .map_err(window_error),
    }
}

fn window_error(error: WindowError) -> CkError {
    if error.exit == 2 {
        CkError::Usage(error.message)
    } else {
        CkError::Message(error.message)
    }
}

fn close(
    output: JsonOutput,
    verb: &str,
    result: Result<Close, WindowError>,
) -> Result<(), CkError> {
    let result = match result {
        Ok(closed) => closed.into_result(),
        Err(error) => error.into(),
    };
    output.completion(&result, result.exit, || {
        let mut lines = Vec::new();
        if let Some(id) = &result.window {
            lines.push(format!("{verb} window {id}."));
        }
        if let Some(message) = &result.message {
            lines.push(message.clone());
        }
        for step in &result.outstanding {
            lines.push(format!(
                "Step {} ({}) has no durable undo mark: {}",
                step.index,
                step.kind,
                step.target.display()
            ));
        }
        lines.extend(
            result
                .warnings
                .iter()
                .map(|warning| format!("Warning: {warning}")),
        );
        lines.join("\n")
    })
}
