//! Text shared by the CLI and terminal upgrade views. Rendering here never
//! writes to stdout, so each caller controls where the same words appear.

use super::{ManagedUpgradeTarget, PlanOutcome};

/// Refusals already contain their actionable wording; do not prefix it again.
pub fn human_blocking_outcome(outcome: &PlanOutcome) -> String {
    match outcome {
        PlanOutcome::Refusal { reason } | PlanOutcome::TargetRefused { reason, .. } => {
            reason.clone()
        }
        _ => outcome.to_string(),
    }
}

/// Keep diagnostic lines, including their newlines, separate from the summary.
pub fn print_verbose_outcomes(outcomes: &[PlanOutcome], verbose: bool) -> String {
    if !verbose {
        return String::new();
    }
    outcomes
        .iter()
        .map(|outcome| format!("  outcome: {outcome}\n"))
        .collect()
}

/// Put the CLI first and omit the gateway's version, which is not self-reported.
pub fn upgrade_current_summary(discovered: &[ManagedUpgradeTarget]) -> String {
    let mut components = Vec::new();
    let ordered = discovered
        .iter()
        .filter(|item| item.target.is_self_replacing())
        .chain(
            discovered
                .iter()
                .filter(|item| !item.target.is_self_replacing()),
        );
    for item in ordered {
        let target = item.target;
        if target.label() == "ck-subc-mcp" {
            components.push(target.to_string());
        } else {
            components.push(format!("{target} {}", item.installed_version));
        }
    }
    if components.is_empty() {
        "Everything is up to date.".to_string()
    } else {
        format!("Everything is up to date ({}).", components.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_outcome_keeps_the_same_refusal_for_every_view() {
        let reason = "Move the binary aside and run ck setup again.";
        assert_eq!(
            human_blocking_outcome(&PlanOutcome::Refusal {
                reason: reason.into()
            }),
            reason
        );
        assert_eq!(
            human_blocking_outcome(&PlanOutcome::TargetRefused {
                component: super::super::Component::Core,
                reason: reason.into(),
            }),
            reason
        );
        assert_eq!(
            print_verbose_outcomes(
                &[PlanOutcome::Refusal {
                    reason: reason.into()
                }],
                true
            ),
            format!("  outcome: refusal: {reason}\n")
        );
        assert_eq!(print_verbose_outcomes(&[], false), "");
    }
}
