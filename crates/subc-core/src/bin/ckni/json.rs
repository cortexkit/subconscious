//! Explicit JSON rendering policy, kept separate from terminal detection.

use serde_json::{json, Value};

use super::super::{CkError, Command, ModuleCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonForm {
    Schema(&'static str),
    None(&'static str),
    Exempt,
}

pub(crate) fn json_form(command: &Command) -> JsonForm {
    match command {
        Command::Health => JsonForm::Schema("ck.health/v1"),
        Command::HealthDetail { .. } => JsonForm::Schema("ck.health.detail/v1"),
        Command::Activity(_) => JsonForm::Schema("ck.activity/v1"),
        Command::Module(module) => match module {
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
        Command::MachineShow => JsonForm::Schema("ck.machine.show/v1"),
        Command::MachineAdopt { .. } => JsonForm::Schema("ck.machine.adopt/v1"),
        Command::Quota { .. } => JsonForm::Schema("ck.quota/v1"),
        Command::Setup(_) => JsonForm::None("setup"),
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
        | Command::CliFaceReservedNames
        | Command::Dashboard
        | Command::External { .. } => JsonForm::Exempt,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonOutput {
    pub(crate) enabled: bool,
    schema: Option<&'static str>,
    stream: bool,
}

impl From<bool> for JsonOutput {
    fn from(enabled: bool) -> Self {
        Self {
            enabled,
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
        let schema = if enabled && ckni {
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

    use super::*;
    use crate::ckni::{plain_output, PlainOutput, Table};

    #[test]
    fn every_builtin_variant_has_one_plain_and_one_documented_json_classification() {
        let cases = [
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
                "daemon triage",
                PlainOutput::PipedText,
                JsonForm::Schema("ck.daemon.triage/v1"),
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
        let mut schemas = BTreeSet::new();
        let mut walk = |text: &str, plain, json| {
            let args = crate::parse_args(
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
