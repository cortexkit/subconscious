use serde::Serialize;

use crate::stack::{DegradedModule, StackResult};

use super::super::{
    journal::{Action, Snapshot},
    StepSummary, WindowResult,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Reverted,
    RefusedBeforeChange,
    StoppedPartWay,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Reverted => "reverted",
            Self::RefusedBeforeChange => "refused_before_change",
            Self::StoppedPartWay => "stopped_part_way",
        }
    }

    pub const fn exit(self) -> i32 {
        match self {
            Self::Reverted => 0,
            Self::RefusedBeforeChange => 2,
            Self::StoppedPartWay => 1,
        }
    }
}

/// The command's completion report. Use `exit()` for the process exit status.
#[derive(Debug, Serialize)]
pub struct RevertResult {
    #[serde(flatten)]
    pub report: WindowResult,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_undone: Option<usize>,
    pub degraded: Vec<DegradedModule>,
    #[serde(skip)]
    stack_human: Option<String>,
}

impl Default for RevertResult {
    fn default() -> Self {
        let mut report = WindowResult::default();
        report.exit = Outcome::RefusedBeforeChange.exit();
        Self {
            report,
            outcome: Outcome::RefusedBeforeChange,
            steps_total: None,
            steps_undone: None,
            degraded: Vec::new(),
            stack_human: None,
        }
    }
}

impl RevertResult {
    pub const fn exit(&self) -> i32 {
        self.outcome.exit()
    }

    pub fn human(&self) -> String {
        let mut lines = vec![format!(
            "{}: {}",
            self.report.window.as_deref().unwrap_or("Window revert"),
            self.outcome.label()
        )];
        if let (Some(undone), Some(total)) = (self.steps_undone, self.steps_total) {
            lines.push(format!("Undone {undone} of {total} steps."));
        }
        if let Some(message) = &self.report.message {
            lines.push(message.clone());
        }
        if let Some(details) = &self.stack_human {
            if !details.is_empty() {
                lines.push(details.clone());
            }
        }
        for path in &self.report.restored {
            lines.push(format!("Restored {}", path.display()));
        }
        lines.extend(
            self.report
                .warnings
                .iter()
                .map(|warning| format!("Warning: {warning}")),
        );
        lines.join("\n")
    }

    pub(super) fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
        self.report.exit = outcome.exit();
    }

    pub(super) fn read_snapshot(&mut self, snapshot: &Snapshot) {
        self.report.window = Some(snapshot.id.clone());
        self.steps_total = Some(snapshot.steps.len());
        self.steps_undone = Some(snapshot.steps_undone());
        self.report.outstanding.clear();
        self.report.restored.clear();
        for (i, entry) in snapshot.steps.iter().enumerate() {
            if !entry.undone {
                self.report.outstanding.push(StepSummary {
                    index: i + 1,
                    kind: entry.step.action.kind(),
                    target: entry.step.target.clone(),
                });
            } else if !matches!(entry.step.action, Action::Activation { .. })
                && !self.report.restored.contains(&entry.step.target)
            {
                self.report.restored.push(entry.step.target.clone());
            }
        }
    }

    pub(super) fn read_stack(&mut self, stack: StackResult, stopping: bool) {
        self.stack_human = Some(stack.human(stopping));
        self.report.message = stack.message;
        self.report.survivors = stack.survivors;
        self.report.unhealthy = stack.unhealthy;
        self.report.not_health_checked = stack.not_health_checked;
        self.degraded = stack.degraded;
        self.report.marker_removed |= stack.marker_removed;
        self.report.daemon_unreachable = stack.daemon_unreachable;
        self.report.daemon_past_bound = stack.daemon_past_bound;
        self.report.relaunched_pid = stack.relaunched_pid;
        self.report.manager_error = stack.manager_error;
        self.report.roster_error = stack.roster_error;
    }

    pub(super) fn stack_message(&self, action: &str) -> String {
        self.report.message.as_ref().map_or_else(
            || action.to_owned(),
            |message| format!("{action}: {message}"),
        )
    }
}
