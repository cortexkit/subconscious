//! Explicit JSON rendering policy, kept separate from terminal detection.

use serde_json::{json, Value};

use super::super::{stack_cli, window_cli, CkError, Command, ModuleCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonForm {
    Schema(&'static str),
    None(&'static str),
    Exempt,
}

pub(crate) fn json_form(command: &Command) -> JsonForm {
    match command {
        Command::Config(_) | Command::PolicyRoot(_) => JsonForm::Exempt,
        Command::Stack(action) => match action {
            stack_cli::Action::Status(_) => JsonForm::Schema("ck.stack.status/v1"),
            stack_cli::Action::Stop(_) | stack_cli::Action::Start(_) => {
                JsonForm::Schema("ck.stack.result/v1")
            }
        },
        Command::Window(action) => match action {
            window_cli::Action::List => JsonForm::Schema("ck.window.list/v1"),
            window_cli::Action::Keep(_)
            | window_cli::Action::Abandon(_)
            | window_cli::Action::Revert(_) => JsonForm::Schema("ck.window.result/v1"),
            window_cli::Action::Begin(_) => JsonForm::None("window begin"),
            window_cli::Action::Current => JsonForm::None("window current"),
            window_cli::Action::Record { .. } => JsonForm::None("window record"),
            window_cli::Action::Done { .. } => JsonForm::None("window done"),
        },
        Command::Health => JsonForm::Schema("ck.health/v1"),
        Command::HealthDetail { .. } => JsonForm::Schema("ck.health.detail/v1"),
        Command::Activity(_) => JsonForm::Schema("ck.activity/v1"),
        Command::Module(module) => match module {
            ModuleCommand::CheckScript { .. } => JsonForm::None("module check-script"),
            ModuleCommand::List => JsonForm::Schema("ck.module.list/v1"),
            ModuleCommand::Resources => JsonForm::Schema("ck.module.resources/v1"),
            ModuleCommand::Profile { .. } => JsonForm::Schema("ck.module.profile/v1"),
            ModuleCommand::Status { .. } => JsonForm::Schema("ck.module.status/v1"),
            ModuleCommand::Logs { .. } => JsonForm::Schema("ck.module.logs/v1"),
            ModuleCommand::StderrTail { .. } => JsonForm::Schema("ck.module.stderr/v1"),
            ModuleCommand::Terminals { .. } => JsonForm::Schema("ck.module.terminals/v1"),
            ModuleCommand::Rescan { .. } => JsonForm::Schema("ck.module.rescan/v1"),
            ModuleCommand::ReleaseReserved { .. } => JsonForm::Schema("ck.module.release/v1"),
            ModuleCommand::Restart { .. }
            | ModuleCommand::Stop { .. }
            | ModuleCommand::Start { .. }
            | ModuleCommand::Swap { .. } => JsonForm::Exempt,
        },
        Command::Routes { .. } => JsonForm::Schema("ck.routes/v1"),
        Command::Catalog { .. } => JsonForm::Schema("ck.catalog/v1"),
        Command::Provenance { .. } => JsonForm::Schema("ck.provenance/v1"),
        Command::Daemon => JsonForm::Schema("ck.daemon/v1"),
        Command::DaemonTriage => JsonForm::Schema("ck.daemon.triage/v1"),
        Command::Doctor(options) if options.agent => JsonForm::None("doctor --agent"),
        Command::Doctor(_) => JsonForm::Schema("ck.doctor.report/v1"),
        Command::MachineShow => JsonForm::Schema("ck.machine.show/v1"),
        Command::MachineAdopt { .. } => JsonForm::Schema("ck.machine.adopt/v1"),
        Command::Quota { .. } => JsonForm::Schema("ck.quota/v1"),
        Command::Setup(_) | Command::SetupExtra(_) => JsonForm::None("setup"),
        Command::Upgrade { .. } => JsonForm::None("upgrade"),
        Command::FleetLint { .. } => JsonForm::None("daemon lint"),
        Command::Help(text) => {
            JsonForm::None(if text == &format!("ck {}", env!("CARGO_PKG_VERSION")) {
                "version"
            } else {
                "help"
            })
        }
        Command::BuildShape
        | Command::DaemonAnnounceRestart
        | Command::DaemonUpgradeInPlace
        | Command::CliFaceReservedNames
        | Command::Dashboard
        | Command::External { .. } => JsonForm::Exempt,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonOutput {
    pub(crate) enabled: bool,
    pub(crate) ckni: bool,
    schema: Option<&'static str>,
    stream: bool,
}

impl From<bool> for JsonOutput {
    fn from(enabled: bool) -> Self {
        Self {
            enabled,
            ckni: false,
            schema: None,
            stream: false,
        }
    }
}

impl JsonOutput {
    pub(crate) fn for_command(
        command: &Command,
        enabled: bool,
        ckni: bool,
    ) -> Result<Self, CkError> {
        let schema = if enabled
            && (ckni
                || matches!(
                    command,
                    Command::Stack(_) | Command::Window(_) | Command::Doctor(_)
                )) {
            match json_form(command) {
                JsonForm::Schema(schema) => Some(schema),
                JsonForm::None(command) => {
                    return Err(CkError::Usage(format!("{command} has no JSON form")));
                }
                JsonForm::Exempt => None,
            }
        } else {
            None
        };
        let stream = schema.is_some()
            && matches!(command, Command::Module(ModuleCommand::Logs { options, .. }) if options.follow);
        Ok(Self {
            enabled,
            ckni,
            schema,
            stream,
        })
    }

    pub(crate) fn value(self, value: &Value) -> Value {
        let Some(schema) = self.schema else {
            return value.clone();
        };
        match value {
            Value::Object(fields) => {
                let mut fields = fields.clone();
                fields.insert("schema".into(), schema.into());
                Value::Object(fields)
            }
            Value::Array(_) => json!({"schema": schema, "entries": value}),
            _ => unreachable!("built-in JSON outputs are objects or arrays"),
        }
    }

    pub(crate) fn print(self, value: &Value) -> Result<(), CkError> {
        if self.stream {
            println!("{}", self.stream_line(value)?);
            return Ok(());
        }
        super::super::print_json(&self.value(value))
    }

    pub(crate) fn completion(
        self,
        report: &impl serde::Serialize,
        exit_code: i32,
        human: impl FnOnce() -> String,
    ) -> Result<(), CkError> {
        if self.enabled {
            self.print(&serde_json::to_value(report)?)?;
        } else {
            println!("{}", human());
        }
        if exit_code == 0 {
            Ok(())
        } else {
            Err(CkError::StackWindowExit { exit_code })
        }
    }

    pub(crate) fn stream_line(self, value: &Value) -> Result<String, CkError> {
        Ok(serde_json::to_string(&self.value(value))?)
    }

    pub(crate) fn is_versioned(self) -> bool {
        self.schema.is_some()
    }

    pub(crate) fn is_stream(self) -> bool {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashSet},
        ffi::OsString,
        mem::discriminant,
    };

    use super::super::{plain_output, PlainOutput, Table};
    use super::*;

    #[test]
    fn versioned_module_json_retains_tree_verbatim() {
        let reading = json!({"source":"process_tree","memory_bytes":8192,"process_count":4});
        let output =
            JsonOutput::for_command(&Command::Module(ModuleCommand::List), true, true).unwrap();
        let rendered = output.value(&json!({"modules":[{"module_id":"m", "tree":reading}]}));
        assert_eq!(rendered["schema"], "ck.module.list/v1");
        assert_eq!(rendered["modules"][0]["tree"], reading);
        assert!(rendered["modules"][0]["tree"].get("swap_bytes").is_none());
    }

    #[test]
    fn every_builtin_variant_has_one_plain_and_one_documented_json_classification() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let apply = format!("config apply {digest}");
        let revert = format!("config revert {digest}");
        let place_program = format!("config place-program alpha /program {digest}");
        let cases = [
            // Config replies retain the daemon's complete view instead of a
            // ckni schema envelope, while plain output keeps piped ck's form.
            ("config status", PlainOutput::PipedText, JsonForm::Exempt),
            ("config plan", PlainOutput::PipedText, JsonForm::Exempt),
            (apply.as_str(), PlainOutput::PipedText, JsonForm::Exempt),
            (revert.as_str(), PlainOutput::PipedText, JsonForm::Exempt),
            ("config stamp", PlainOutput::PipedText, JsonForm::Exempt),
            ("config unstamp", PlainOutput::PipedText, JsonForm::Exempt),
            ("config history", PlainOutput::PipedText, JsonForm::Exempt),
            (
                "config rollback 4",
                PlainOutput::PipedText,
                JsonForm::Exempt,
            ),
            (
                place_program.as_str(),
                PlainOutput::PipedText,
                JsonForm::Exempt,
            ),
            // Policy-root replies are the daemon's own replies, printed as piped ck
            // prints them, like the config replies above.
            ("policy-root show", PlainOutput::PipedText, JsonForm::Exempt),
            (
                "stack status",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.stack.status/v1"),
            ),
            (
                "stack stop",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.stack.result/v1"),
            ),
            (
                "stack start",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.stack.result/v1"),
            ),
            (
                "window list",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.window.list/v1"),
            ),
            (
                "window keep",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.window.result/v1"),
            ),
            (
                "window abandon 20261123-010203-abcd",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.window.result/v1"),
            ),
            (
                "window revert",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.window.result/v1"),
            ),
            (
                "window begin",
                PlainOutput::PipedText,
                JsonForm::None("window begin"),
            ),
            (
                "window current",
                PlainOutput::PipedText,
                JsonForm::None("window current"),
            ),
            (
                "window record file --target /target --remove --window 20261123-010203-abcd --owner-pid 1",
                PlainOutput::PipedText,
                JsonForm::None("window record"),
            ),
            (
                "window done 1 --window 20261123-010203-abcd --owner-pid 1",
                PlainOutput::PipedText,
                JsonForm::None("window done"),
            ),
            (
                "module resources",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.resources/v1"),
            ),
            (
                "module profile alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.profile/v1"),
            ),
            (
                "health",
                PlainOutput::Table(Table::Health),
                JsonForm::Schema("ck.health/v1"),
            ),
            (
                "module list",
                PlainOutput::Table(Table::Modules),
                JsonForm::Schema("ck.module.list/v1"),
            ),
            (
                "activity",
                PlainOutput::Table(Table::Activity),
                JsonForm::Schema("ck.activity/v1"),
            ),
            (
                "health alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.health.detail/v1"),
            ),
            (
                "module status alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.status/v1"),
            ),
            (
                "module check-script bundle.js",
                PlainOutput::PipedText,
                JsonForm::None("module check-script"),
            ),
            (
                "module logs",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.logs/v1"),
            ),
            (
                "module stderr alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.stderr/v1"),
            ),
            (
                "module terminals alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.terminals/v1"),
            ),
            (
                "module rescan --dry-run",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.rescan/v1"),
            ),
            (
                "module release alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.module.release/v1"),
            ),
            (
                "routes",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.routes/v1"),
            ),
            (
                "catalog",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.catalog/v1"),
            ),
            (
                "provenance alpha",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.provenance/v1"),
            ),
            (
                "daemon",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.daemon/v1"),
            ),
            (
                "daemon upgrade --in-place",
                PlainOutput::PipedText,
                JsonForm::Exempt,
            ),
            (
                "daemon triage",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.daemon.triage/v1"),
            ),
            (
                "doctor",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.doctor.report/v1"),
            ),
            (
                "doctor --json",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.doctor.report/v1"),
            ),
            (
                "doctor --fix --report report.json",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.doctor.report/v1"),
            ),
            (
                "doctor --subc rig.json",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.doctor.report/v1"),
            ),
            (
                "doctor --agent",
                PlainOutput::PipedText,
                JsonForm::None("doctor --agent"),
            ),
            (
                "machine show",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.machine.show/v1"),
            ),
            (
                "machine adopt example --force",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.machine.adopt/v1"),
            ),
            (
                "quota",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.quota/v1"),
            ),
            (
                "setup --dry-run",
                PlainOutput::PipedText,
                JsonForm::None("setup"),
            ),
            (
                "setup operator-prompt",
                PlainOutput::PipedText,
                JsonForm::None("setup"),
            ),
            (
                "upgrade --check",
                PlainOutput::PipedText,
                JsonForm::None("upgrade"),
            ),
            (
                "daemon lint",
                PlainOutput::PipedText,
                JsonForm::None("daemon lint"),
            ),
            ("--help", PlainOutput::PipedText, JsonForm::None("help")),
        ];
        // Machine probes are read by setup and must be byte-identical. The app
        // is refused, external faces own their output, and action acknowledgements
        // retain piped ck's plain and JSON forms for existing automation.
        let exemptions = [
            ("daemon announce-restart", "script-only raw acknowledgement"),
            ("--ck-build-shape", "machine probe read by setup"),
            (
                "--ck-cli-face-reserved-names",
                "machine probe read by setup",
            ),
            ("", "dashboard is refused; never opens the app"),
            (
                "external-face arg",
                "external face output is passed through",
            ),
            ("module restart alpha", "piped action acknowledgement"),
            ("module restart alpha --swap", "piped swap acknowledgement"),
            ("module stop alpha", "piped action acknowledgement"),
            ("module start alpha", "piped action acknowledgement"),
        ];
        let docs = include_str!("../../../../../docs/ck-json.md");
        let mut commands = HashSet::new();
        let mut modules = HashSet::new();
        let mut configs = HashSet::new();
        let mut stacks = HashSet::new();
        let mut windows = HashSet::new();
        let mut schemas = BTreeSet::new();
        let mut walk = |text: &str, plain, json| {
            let args = super::super::super::parse_args(
                std::iter::once(OsString::from("ckni"))
                    .chain(text.split_whitespace().map(OsString::from)),
            )
            .unwrap();
            assert_eq!(plain_output(&args.command), plain, "{text}");
            assert_eq!(json_form(&args.command), json, "{text}");
            commands.insert(discriminant(&args.command));
            if let Command::Module(module) = &args.command {
                assert!(
                    modules.insert(discriminant(module)),
                    "duplicate module variant: {text}"
                );
            }
            if let Command::Config(config) = &args.command {
                assert!(
                    configs.insert(discriminant(config)),
                    "duplicate config variant: {text}"
                );
            }
            if let Command::Stack(action) = &args.command {
                assert!(
                    stacks.insert(discriminant(action)),
                    "duplicate stack action: {text}"
                );
            }
            if let Command::Window(action) = &args.command {
                assert!(
                    windows.insert(discriminant(action)),
                    "duplicate window action: {text}"
                );
            }
            match json {
                JsonForm::Schema(schema) => {
                    assert_eq!(
                        docs.matches(&format!("### `{schema}`")).count(),
                        1,
                        "{schema}"
                    );
                    schemas.insert(schema);
                }
                JsonForm::None(name) => assert!(
                    docs.contains(&format!("| `{name}` | no JSON form |")),
                    "{name}"
                ),
                JsonForm::Exempt => {}
            }
        };
        for (text, plain, json) in cases {
            walk(text, plain, json);
        }
        for (text, reason) in exemptions {
            assert!(!reason.is_empty());
            walk(text, PlainOutput::Exempt, JsonForm::Exempt);
        }
        // Count declarations independently of the fixture list. Exhaustive policy
        // matches catch new variants at compile time; this also catches a missing
        // walk entry when someone adds the policy but forgets its documentation.
        let source = include_str!("../ck.rs");
        assert_eq!(
            commands.len(),
            variant_count(source, "Command"),
            "unclassified Command variant"
        );
        assert_eq!(
            modules.len(),
            variant_count(source, "ModuleCommand"),
            "unclassified ModuleCommand variant"
        );
        assert_eq!(
            stacks.len(),
            variant_count(include_str!("../stack_cli/mod.rs"), "Action"),
            "unclassified stack action"
        );
        assert_eq!(
            windows.len(),
            variant_count(include_str!("../window_cli/mod.rs"), "Action"),
            "unclassified window action"
        );
        // Settings pages send these requests; `ck config` has no command that
        // produces them, so they have no CLI output to classify. Naming them
        // keeps the count exact: a new request variant still fails here until
        // it gets a `ck config` form above or joins this list on purpose.
        let settings_page_only = [
            "Modules",
            "Schema",
            "Show",
            "Watch",
            "PlanGet",
            "PlanDiscard",
        ];
        let config_source = include_str!("../../../../subc-control/src/config.rs");
        for name in settings_page_only {
            assert!(
                config_source.contains(&format!("    {name} {{")),
                "{name} is not a ConfigControlRequest variant"
            );
        }
        assert_eq!(
            configs.len() + settings_page_only.len(),
            variant_count(config_source, "ConfigControlRequest"),
            "unclassified ConfigControlRequest variant"
        );
        let documented = docs
            .lines()
            .filter_map(|line| {
                line.strip_prefix("### `")
                    .and_then(|rest| rest.strip_suffix('`'))
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            schemas, documented,
            "orphan or missing schema documentation"
        );
    }

    fn variant_count(source: &str, name: &str) -> usize {
        // A Windows checkout may have CRLF line endings, so the text is
        // normalized before it is split on "\n".
        let source = source.replace("\r\n", "\n");
        let body = source
            .split_once(&format!("enum {name} {{\n"))
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let variants = body
            .lines()
            .filter(|line| {
                line.strip_prefix("    ")
                    .and_then(|line| line.chars().next())
                    .is_some_and(|c| c.is_ascii_uppercase())
            })
            .count();
        assert!(variants > 0, "enum declarations must be found");
        variants
    }

    #[test]
    fn json_policy_versions_objects_arrays_and_each_stream_line_without_changing_ck() {
        let versioned = JsonOutput {
            enabled: true,
            ckni: true,
            schema: Some("ck.activity/v1"),
            stream: false,
        };
        let object = json!({"seq": 42, "details": {"schema": "nested/v2"}});
        assert_eq!(
            versioned.value(&object),
            json!({"schema":"ck.activity/v1", "seq":42, "details":{"schema":"nested/v2"}})
        );
        assert_eq!(
            versioned.value(&json!([object.clone()])),
            json!({"schema":"ck.activity/v1", "entries":[object.clone()]})
        );
        assert_eq!(
            versioned.value(&json!([])),
            json!({"schema":"ck.activity/v1", "entries":[]})
        );
        let line = versioned.stream_line(&object).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["schema"],
            "ck.activity/v1"
        );
        assert_eq!(JsonOutput::from(true).value(&object), object);
    }
}
