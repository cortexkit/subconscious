//! Data shared by the human, JSON, and file reports.

use std::path::PathBuf;

use serde::Serialize;

/// Final diagnostic document, full tails, and the process exit code.
#[derive(Debug)]
pub struct DoctorRun {
    /// The document printed by `--json`.
    pub document: DoctorReport,
    /// Complete bounded tail records for the report file.
    pub log_tails: Vec<LogTailRecord>,
    /// Exit code, including the result of any attempted fix.
    pub exit_code: u8,
}

/// The `ck.doctor.report/v1` document, without the full log tails.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    /// Document schema identifier.
    pub schema: &'static str,
    /// Whether the final pass found no errors.
    pub ok: bool,
    /// Whether `server.describe` answered in the final pass.
    pub daemon_answered: bool,
    /// Explicit connection-file path, if any.
    pub connection_override: Option<PathBuf>,
    /// Final-pass findings, in display order.
    pub findings: Vec<Finding>,
    /// Revert offered for the final pass, if any.
    pub offer: Option<Offer>,
    /// File actually written, if any.
    pub report: Option<ReportFile>,
    /// Revert attempted or skipped by `--fix`.
    pub fix: Option<FixReport>,
    /// Program outcomes, omitted by the stack-only doctor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<ModuleCheck>>,
}

/// Path and privacy status of the full report written locally.
#[derive(Debug, Serialize)]
pub struct ReportFile {
    /// The actual path, including any collision suffix.
    pub path: PathBuf,
    /// Whether private data was removed from the report.
    pub sanitized: bool,
}

/// Suggested recovery of a local restart window.
#[derive(Debug, Serialize)]
pub struct Offer {
    /// Window that can be reverted.
    pub window_id: String,
    /// The command that applies the revert.
    pub command: String,
}

/// How `--fix` handled the first pass.
#[derive(Debug, Serialize)]
pub struct FixReport {
    /// The `ck.window.result/v1` document if the revert was invoked.
    pub result: Option<crate::window::RevertResult>,
    /// Why no revert was attempted, if it wasn't invoked.
    pub skipped: Option<FixSkip>,
    /// The first pass's findings, even if a second pass ran.
    pub findings_before: Vec<Finding>,
    /// Decisions and results in module fix application order.
    pub module_fixes: Vec<ModuleFix>,
}

/// Outcome of asking one configured module for its own diagnostics.
#[derive(Debug, Serialize)]
pub struct ModuleCheck {
    pub id: String,
    pub outcome: ModuleOutcome,
    pub findings: usize,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleOutcome {
    Findings,
    Clean,
    NoChecks,
    SpawnFailed,
    Crashed,
    TimedOut,
    InvalidOutput,
    NotStarted,
    NotAsked,
    UnsupportedPlatform,
}

/// Decision, result, and enabledness restoration for one offered module fix.
#[derive(Debug, Serialize)]
pub struct ModuleFix {
    pub module: String,
    pub finding: String,
    pub class: cortexkit_doctor::FixClass,
    pub decision: ModuleFixDecision,
    pub skip_reason: Option<ModuleFixSkip>,
    pub result: Option<ModuleFixResult>,
    pub restart: Option<ModuleRestart>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleFixDecision {
    Applied,
    Declined,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleFixSkip {
    NoTerminal,
    RevertFirst,
    ManagerObservationOverdue,
    StopFailed,
    ProgramChanged,
    Interrupted,
    SignalSetupFailed,
}

#[derive(Debug, Serialize)]
pub struct ModuleFixResult {
    pub outcome: ModuleFixOutcome,
    pub code: Option<String>,
    pub evidence: Option<Vec<cortexkit_doctor::Evidence>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleFixOutcome {
    Fixed,
    Refused,
    Failed,
    NoResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleRestart {
    Enabled,
    Failed,
}

/// Reason an explicitly requested revert was not attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixSkip {
    NothingToFix,
    ManagerObservationOverdue,
    Interrupted,
}

/// Diagnostic layer, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Install,
    Service,
    Daemon,
    Disk,
    Window,
    Module,
    ModuleProgram,
}

impl Layer {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Service => "service",
            Self::Daemon => "daemon",
            Self::Disk => "disk",
            Self::Window => "window",
            Self::Module => "module",
            Self::ModuleProgram => "module_program",
        }
    }
}

/// How serious the finding is, in worst-first order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One catalogue finding and the observations behind it.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    /// Stable catalogue identifier.
    pub id: String,
    /// The component of the stack examined.
    pub layer: Layer,
    /// How serious the finding is.
    pub severity: Severity,
    /// The responsible module id, or `ck` for stack and fleet-wide checks.
    pub owner: String,
    /// A one-line explanation for operators.
    pub summary: String,
    /// Catalogue-specific structured fields.
    pub evidence: serde_json::Value,
    /// The validated fix declaration of a program finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<cortexkit_doctor::Fix>,
    #[serde(skip)]
    subject: String,
    #[serde(skip)]
    check: &'static str,
    #[serde(skip)]
    module: Option<String>,
}

impl Finding {
    pub(super) fn new(
        id: impl Into<String>,
        layer: Layer,
        severity: Severity,
        summary: impl Into<String>,
        evidence: impl Serialize,
    ) -> Self {
        Self {
            id: id.into(),
            layer,
            severity,
            owner: "ck".into(),
            summary: one_line(&summary.into()),
            evidence: serde_json::to_value(evidence)
                .expect("findings use JSON-compatible evidence"),
            fix: None,
            subject: String::new(),
            check: "",
            module: None,
        }
    }

    pub(super) fn for_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    pub(super) fn for_module(mut self, module: &str) -> Self {
        self.owner = module.into();
        self.module = Some(module.into());
        self
    }

    pub(super) fn check(
        layer: Layer,
        severity: Severity,
        timed_out: bool,
        check: &'static str,
        module: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        let outcome = if timed_out {
            "timed_out"
        } else {
            "check_failed"
        };
        let message = message.into();
        let mut finding = Self::new(
            format!("{}.{outcome}", layer.name()),
            layer,
            severity,
            format!("{check}: {}", one_line(&message)),
            serde_json::json!({"check": check, "module": module, "message": message}),
        );
        finding.check = check;
        if let Some(module) = module {
            finding = finding.for_module(module);
        }
        finding
    }

    pub(super) fn sort(findings: &mut [Self]) {
        findings.sort_by(|a, b| {
            if a.layer == Layer::ModuleProgram && b.layer == Layer::ModuleProgram {
                (&a.owner, a.severity, &a.id).cmp(&(&b.owner, b.severity, &b.id))
            } else {
                a.sort_key().cmp(&b.sort_key())
            }
        });
    }

    fn sort_key(&self) -> (Layer, Severity, &str, &str, &str, &str, Option<&str>) {
        (
            self.layer,
            self.severity,
            &self.id,
            &self.owner,
            &self.subject,
            self.check,
            self.module.as_deref(),
        )
    }
}

pub(super) fn one_line(message: &str) -> String {
    message.lines().collect::<Vec<_>>().join(" ")
}

/// Source of a module's bounded log-tail record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TailSource {
    StderrTail,
    LogFile,
}

/// Whether a log tail was collected and, if so, its capture state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TailStatus {
    Captured,
    Incomplete,
    NotCaptured,
    Unknown,
    RequestFailed,
    RequestTimedOut,
    Read,
    Absent,
    Unreadable,
    NotRead,
}

/// A line or a marker in a log-tail reading.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TailEntry {
    Line {
        /// Text as captured, without interpreting its contents.
        text: String,
        /// Unix timestamp in milliseconds when supplied separately.
        #[serde(skip_serializing_if = "Option::is_none")]
        at_ms: Option<u64>,
        /// Whether the daemon truncated this line.
        truncated: bool,
    },
    ProcessStart,
    Unknown,
}

/// Full bounded tail, including status even when there are no lines.
#[derive(Clone, Debug, Serialize)]
pub struct LogTailRecord {
    /// Module id.
    pub module: String,
    /// Where the tail came from.
    pub source: TailSource,
    /// Read or capture status.
    pub status: TailStatus,
    /// Capture reason, read error, or non-fatal warning, if any.
    pub reason: Option<String>,
    /// Lines omitted by the daemon, if reported.
    pub dropped_lines: Option<u64>,
    /// Whether the local byte window cut the file reading short.
    pub truncated: bool,
    /// Entries in the order received or read.
    pub entries: Vec<TailEntry>,
}

impl LogTailRecord {
    pub(super) fn empty(module: &str, source: TailSource, status: TailStatus) -> Self {
        Self {
            module: module.into(),
            source,
            status,
            reason: None,
            dropped_lines: None,
            truncated: false,
            entries: Vec::new(),
        }
    }

    pub(super) fn preview(&self) -> Self {
        let start = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(i, entry)| matches!(entry, TailEntry::Line { .. }).then_some(i))
            .nth(4)
            .unwrap_or_default();
        Self {
            entries: self.entries[start..].to_vec(),
            ..self.clone()
        }
    }
}
