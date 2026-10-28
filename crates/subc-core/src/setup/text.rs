//! Text shared by the CLI and terminal upgrade views. Rendering here never
//! writes to stdout, so each caller controls where the same words appear.

use super::{ManagedUpgradeTarget, PlanOutcome, UpgradePlan};

pub(super) const MC_UNPUBLISHED_NOTE: &str = concat!(
    "Magic Context's module (ck-mc) isn't published for public installs yet. Magic Context runs fully inside its OpenCode and Pi plugin, so you don't need the module. Install the plugin with:\n",
    "  OpenCode:    npx @cortexkit/magic-context@latest setup\n",
    "  Pi:          npx @cortexkit/magic-context@latest setup --harness pi\n",
    "  Oh My Pi:    npx @cortexkit/magic-context@latest setup --harness omp\n",
    "The module will install through `ck setup mc` once a release is published.",
);

/// Setup leaves unfamiliar programs in place and names the safe next step.
pub fn unowned_binary_refusal(path: &std::path::Path) -> String {
    format!(
        "ck setup did not install {}, so it won't replace it. \
         Move it aside and run ck setup again. Nothing was changed.",
        super::components::display_home_path(path)
    )
}

/// Group paths so the explanation and next step appear only once.
pub fn upgrade_unowned_binaries_refusal(paths: &[std::path::PathBuf]) -> String {
    let paths = paths
        .iter()
        .map(|path| format!("  {}", super::components::display_home_path(path)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "ck upgrade manages only what ck setup installed. ck setup did not install these, so ck upgrade won't replace them:\n{paths}\nMove them aside and run ck setup again. Nothing was changed."
    )
}

pub fn upgrade_nothing_managed_refusal() -> String {
    "No CortexKit programs installed by ck setup were found. ck upgrade manages only what ck setup installed. Run ck setup to install them or take over the ones already here. Nothing was changed.".to_string()
}

/// Refusals already contain their actionable wording; do not prefix it again.
pub fn human_blocking_outcome(outcome: &PlanOutcome) -> String {
    match outcome {
        PlanOutcome::Refusal { reason } | PlanOutcome::TargetRefused { reason, .. } => {
            reason.clone()
        }
        _ => outcome.to_string(),
    }
}

/// What `ck upgrade` prints, and the terminal app's update wizard shows,
/// when ownership stops an upgrade before anything is checked: each refusal
/// on its own line. `None` when nothing refuses.
pub fn upgrade_ownership_refusal(outcomes: &[PlanOutcome]) -> Option<String> {
    (!outcomes.is_empty()).then(|| {
        outcomes
            .iter()
            .map(human_blocking_outcome)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// What `ck upgrade` prints, and the update wizard shows, when the plan
/// itself is refused: its first blocking outcome. `None` for a plan that may
/// run.
pub fn upgrade_plan_refusal(plan: &UpgradePlan) -> Option<String> {
    (!plan.is_authorized()).then(|| {
        plan.outcomes
            .iter()
            .find(|outcome| outcome.blocks_execution())
            .map(human_blocking_outcome)
            .unwrap_or_else(|| "upgrade could not continue; nothing was changed".to_string())
    })
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

/// Put the CLI first; the gateway's label has no comparable release version.
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
        upgrade_nothing_managed_refusal()
    } else {
        format!("Everything is up to date ({}).", components.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_upgrade_summary_does_not_claim_currency() {
        assert_eq!(upgrade_current_summary(&[]), "No CortexKit programs installed by ck setup were found. ck upgrade manages only what ck setup installed. Run ck setup to install them or take over the ones already here. Nothing was changed.");
    }

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
