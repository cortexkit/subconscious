#[cfg(feature = "test-support")]
use std::env;
use std::{collections::BTreeSet, path::Path};

use subc_core::setup::{self, text::upgrade_current_summary};

use super::{discover_connection_file, fetch_supervised_roster, CkError};

pub(super) async fn upgrade_command(
    executable: &Path,
    subc: Option<&Path>,
    check: bool,
    verbose: bool,
    dry_run: bool,
) -> Result<(), CkError> {
    // Keep the discovered path for post-restart verification (core versions come from the binary).
    let connection = discover_connection_file(subc).ok();
    let connection_path = subc
        .map(Path::to_path_buf)
        .or_else(|| connection.as_ref().map(|found| found.path.clone()));
    let (_install_hold, discovered) =
        setup::lock_and_discover_upgrade_targets(executable, subc, check || dry_run)
            .map_err(CkError::rejected)?;
    let ownership_outcomes =
        setup::upgrade_ownership_outcomes(executable, &discovered).map_err(CkError::rejected)?;
    for outcome in &ownership_outcomes {
        if let setup::PlanOutcome::SetupNote { message } = outcome {
            println!("{message}");
        }
    }
    if let Some(refusal) = setup::text::upgrade_ownership_refusal(&ownership_outcomes) {
        println!("{refusal}");
        return Err(CkError::RenderedExit { exit_code: 1 });
    }
    if !discovered
        .iter()
        .any(|item| item.target.binary == "ck-auth")
    {
        println!("the vault and message bus are not installed yet; run ck setup");
    }
    let cache = setup::UpdateCache::from_environment();
    let source = setup::IndexReleaseSource::from_environment(setup::TARGET_CHECK_BUDGET);
    let upgrade_roster = discovered
        .iter()
        .map(|item| item.target)
        .collect::<Vec<setup::UpgradeTarget>>();
    let metadata = match setup::check_update_metadata(&cache, &source, &upgrade_roster).await {
        Ok(metadata) => metadata,
        Err(error @ setup::UpdateCheckError::IndexStale { .. }) => {
            println!("{error}");
            return Err(CkError::UpdateCheck(error));
        }
        Err(error @ setup::UpdateCheckError::IndexUnreachable { .. }) => {
            println!("{}", setup::not_checked_from_cache(&cache).render());
            return Err(CkError::UpdateCheck(error));
        }
        Err(error) => return Err(CkError::UpdateCheck(error)),
    };
    // Availability is a signed-index observation, not an activation attempt.
    // A stopped daemon must not prevent a read-only update check.
    let roster = if check {
        Ok(BTreeSet::new())
    } else {
        fetch_supervised_roster(subc).await
    };
    let cerebellum = cfg!(target_os = "macos")
        .then(|| {
            discovered
                .iter()
                .find(|item| item.target.module_id() == Some("cerebellum"))
        })
        .flatten();
    let index = if !check || cerebellum.is_some() {
        Some(
            source
                .cloned_index()
                .map_err(|error| CkError::Message(error.to_string()))?,
        )
    } else {
        None
    };
    let planning_index = if check { None } else { index.as_ref() };
    let observed = setup::observed_upgrade_targets(&metadata, &discovered, roster, planning_index);
    let plan = setup::plan_upgrade(&observed);
    if plan.is_authorized() {
        if let Some((module, index)) = cerebellum.zip(index.as_ref()) {
            if module
                .chrome_extension_refresh_needed(index)
                .map_err(CkError::rejected)?
            {
                println!("run ck setup to refresh the Chrome extension");
            }
        }
    }
    if dry_run {
        println!("{}", plan.render());
        return if plan.is_authorized() {
            Ok(())
        } else {
            Err(CkError::RenderedExit { exit_code: 1 })
        };
    }
    if let Some(message) = setup::text::upgrade_plan_refusal(&plan) {
        println!("{message}");
        print!(
            "{}",
            setup::text::print_verbose_outcomes(&plan.outcomes, verbose)
        );
        return Err(CkError::RenderedExit { exit_code: 1 });
    }
    let planned_mutations = plan
        .operations
        .iter()
        .filter(|operation| operation.mutates())
        .count();
    if check {
        let updates = plan
            .outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                setup::PlanOutcome::UpgradeAvailable {
                    target, from, to, ..
                } => Some(format!(
                    "{}. Run ck upgrade.",
                    setup::version_transition(&target.to_string(), from, to)
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            println!("{}", upgrade_current_summary(&discovered));
        } else {
            for update in updates {
                println!("{update}");
            }
        }
        print!(
            "{}",
            setup::text::print_verbose_outcomes(&plan.outcomes, verbose)
        );
        return Ok(());
    }
    if planned_mutations == 0 {
        println!("{}", upgrade_current_summary(&discovered));
        print!(
            "{}",
            setup::text::print_verbose_outcomes(&plan.outcomes, verbose)
        );
        return Ok(());
    }

    #[cfg(feature = "test-support")]
    if env::var_os("CK_TEST_UPGRADE_APPLY_OK").is_some() {
        for outcome in &plan.outcomes {
            if let setup::PlanOutcome::UpgradeAvailable {
                target, from, to, ..
            } = outcome
            {
                println!("{}", setup::upgraded_line(*target, from, to));
            }
        }
        println!("Done.");
        print!(
            "{}",
            setup::text::print_verbose_outcomes(&plan.outcomes, verbose)
        );
        return Ok(());
    }

    let index = index.expect("non-check upgrade loaded its planning index");
    let mut backend = setup::SystemUpgradeBackend::new(
        executable,
        connection_path,
        discovered,
        index,
        setup::UpgradeOutput::Terminal,
    )
    .map_err(CkError::rejected)?;
    backend.set_supervised_modules(observed.supervised_modules.clone());
    for &target in &observed.roster {
        if let setup::UpgradeState::UpdateAvailable { from, to, .. } = observed.target_state(target)
        {
            backend.set_planned_from(target, from);
            backend.set_expected_version(target, to);
        }
    }
    match setup::execute_upgrade(&plan, &mut backend) {
        Ok(report) => {
            println!(
                "{}",
                if report.nothing_changed() {
                    "Nothing changed."
                } else {
                    "Done."
                }
            );
            print!(
                "{}",
                setup::text::print_verbose_outcomes(&plan.outcomes, verbose)
            );
            if verbose {
                setup::render_execution_report(&report);
            }
            Ok(())
        }
        Err(failure) => {
            setup::render_execution_report(&failure.report);
            Err(CkError::rejected(failure.to_string()))
        }
    }
}
