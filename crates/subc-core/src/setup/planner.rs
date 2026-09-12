use super::{
    conversion::{explicit_conversion_requires_confirmation, selected_components},
    model::{
        Component, ComponentState, ConfigurationState, CoreVersion, DetectionOutcome, PlanOutcome,
        PlatformObservation, ReleaseAvailability, RuntimeState, SetupObserved, SetupOperation,
        SetupRequest, UpgradeObserved, UpgradeOperation, UpgradeState, UpgradeTarget,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlan {
    pub operations: Vec<SetupOperation>,
    pub outcomes: Vec<PlanOutcome>,
}

impl SetupPlan {
    pub fn is_authorized(&self) -> bool {
        !self.outcomes.iter().any(PlanOutcome::blocks_execution)
    }

    pub fn mutation_count(&self) -> usize {
        self.operations
            .iter()
            .filter(|operation| operation.mutates())
            .count()
    }

    /// The dry-run text an operator reads before consenting. `ck setup`
    /// prints this verbatim.
    pub fn render(&self) -> String {
        let mut rendered = String::from("setup plan:\n");
        for (index, operation) in self.operations.iter().enumerate() {
            rendered.push_str(&format!("  {}. {operation}\n", index + 1));
        }
        for outcome in &self.outcomes {
            rendered.push_str(&format!("  outcome: {outcome}\n"));
        }
        rendered
    }
}

/// Plans setup only from observations. The dry-run bit belongs to execution,
/// not planning, which keeps a preview and an authorized execution equivalent
/// when they begin with the same observed state.
pub fn plan_setup(observed: &SetupObserved, request: &SetupRequest) -> SetupPlan {
    let mut plan = SetupPlan {
        operations: vec![SetupOperation::ObservePlatform],
        outcomes: Vec::new(),
    };

    let PlatformObservation::Supported(target) = &observed.platform else {
        if let PlatformObservation::Unsupported(target) = &observed.platform {
            plan.outcomes.push(PlanOutcome::UnsupportedPlatform {
                target: target.clone(),
            });
        }
        return plan;
    };

    if request.uninstall {
        return plan_uninstall(observed, plan);
    }

    if let Some(path) = &observed.running_ck_adoption {
        plan.operations
            .push(SetupOperation::AdoptRunningCk { path: path.clone() });
    }

    record_detection_outcomes(observed, &mut plan);
    let selected = selected_components(request);
    if request.optional_components.is_empty() {
        plan.operations
            .push(SetupOperation::OfferOptionalComponents);
    }

    if let Some(component) = request.convert {
        plan.operations
            .push(SetupOperation::ConfirmConversion { component });
        if explicit_conversion_requires_confirmation(request) {
            plan.outcomes.push(PlanOutcome::Refusal {
                reason: format!(
                    "explicit {component} conversion requires confirmation; re-run with --confirm"
                ),
            });
        }
    }

    let mut newly_configured_modules = Vec::new();
    let mut core_is_being_installed = false;
    for component in selected {
        if let Some(message) = component.unavailable_message(*target) {
            plan.outcomes.push(PlanOutcome::DeclaredUnavailable {
                component,
                message: message.to_string(),
            });
            continue;
        }
        match observed.release(component) {
            ReleaseAvailability::NotYetPublished {
                release_tag,
                missing_asset,
            } => {
                plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                    component,
                    release_tag,
                    missing_asset,
                });
                continue;
            }
            ReleaseAvailability::Incomplete { missing_asset } => {
                plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                    component,
                    release_tag: "the resolved release".to_string(),
                    missing_asset,
                });
                continue;
            }
            ReleaseAvailability::Unresolvable { reason } => {
                plan.outcomes
                    .push(PlanOutcome::ReleaseUnresolvable { component, reason });
                continue;
            }
            ReleaseAvailability::Available | ReleaseAvailability::NotRequired => {}
        }

        let component_state = observed.component_state(component);
        if component.module_id().is_some() && component_state == ComponentState::Missing {
            if let Some(reason) =
                setup_core_floor_refusal(observed, component, core_is_being_installed)
            {
                plan.outcomes
                    .push(PlanOutcome::TargetRefused { component, reason });
                continue;
            }
        }

        match component_state {
            ComponentState::Correct => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{component} is already correct"),
            }),
            ComponentState::Missing => {
                plan.operations
                    .push(SetupOperation::InstallComponent { component });
                if component == Component::Core {
                    core_is_being_installed = true;
                }
                plan.operations
                    .push(SetupOperation::ConfigureComponent { component });
                if component == Component::Claustrum {
                    plan.operations.push(SetupOperation::BootstrapClaustrum {
                        key_path: request.claustrum_key_path.clone(),
                    });
                }
                if component.module_id().is_some() {
                    newly_configured_modules.push(component);
                }
            }
            ComponentState::Configured => {
                plan.outcomes
                    .push(PlanOutcome::ConfiguredNotRegistered { component });
                if component == Component::Claustrum {
                    plan.operations.push(SetupOperation::BootstrapClaustrum {
                        key_path: request.claustrum_key_path.clone(),
                    });
                }
                if component.module_id().is_some() {
                    newly_configured_modules.push(component);
                }
            }
        }
    }

    if matches!(observed.configuration, ConfigurationState::Conflict { .. }) {
        let ConfigurationState::Conflict { key } = &observed.configuration else {
            unreachable!("the match above establishes the conflict variant");
        };
        plan.outcomes.push(PlanOutcome::Refusal {
            reason: format!("user-owned configuration conflicts at key '{key}'"),
        });
    }

    if observed.runtime == RuntimeState::Missing {
        plan.operations.push(SetupOperation::RegisterRuntime);
        plan.operations.push(SetupOperation::StartRuntime);
    } else {
        plan.outcomes.push(PlanOutcome::Noop {
            scope: "per-user runtime is already registered and live".to_string(),
        });
    }

    if !observed.restart_required.is_empty() {
        plan.operations.push(SetupOperation::RestartRuntime {
            sections: observed.restart_required.clone(),
        });
        plan.outcomes.push(PlanOutcome::CoreRestartRequired {
            sections: observed.restart_required.clone(),
        });
    }

    for component in newly_configured_modules {
        plan.operations
            .push(SetupOperation::RescanComponent { component });
        plan.operations
            .push(SetupOperation::EnableComponent { component });
    }

    plan.operations.extend([
        SetupOperation::Validate {
            instrument: "ck daemon triage",
        },
        SetupOperation::Validate {
            instrument: "ck health",
        },
    ]);
    plan
}

fn setup_core_floor_refusal(
    observed: &SetupObserved,
    component: Component,
    core_is_being_installed: bool,
) -> Option<String> {
    let floor = observed.requires_core.get(&component)?;
    let required = match floor.parse::<CoreVersion>() {
        Ok(required) => required,
        Err(()) => {
            return Some(format!(
                "{component} declares malformed requires_core `{floor}`"
            ));
        }
    };
    if core_is_being_installed {
        return None;
    }
    let Some(installed_text) = observed.installed_core_version.as_deref() else {
        return Some(format!(
            "{component} requires core ≥ {floor}, but the installed core version could not be read"
        ));
    };
    let Ok(installed) = installed_text.parse::<CoreVersion>() else {
        return Some(format!(
            "{component} requires core ≥ {floor}, but the installed core version could not be read (`{installed_text}`)"
        ));
    };
    (installed < required).then(|| {
        format!(
            "{component} requires core ≥ {floor}, installed {installed_text}; run `ck upgrade` first"
        )
    })
}

fn record_detection_outcomes(observed: &SetupObserved, plan: &mut SetupPlan) {
    for (component, outcome) in &observed.detections {
        match outcome {
            DetectionOutcome::OwnerGated { reason } => {
                plan.outcomes.push(PlanOutcome::OwnerGatedDetection {
                    component: *component,
                    reason: reason.clone(),
                });
            }
            DetectionOutcome::OfferConversion => {
                plan.operations.push(SetupOperation::OfferConversion {
                    component: *component,
                });
            }
            DetectionOutcome::None
            | DetectionOutcome::InstalledAndLive
            | DetectionOutcome::Unknown => {}
        }
    }
}

fn plan_uninstall(observed: &SetupObserved, mut plan: SetupPlan) -> SetupPlan {
    let mut removals = 0;
    if observed.runtime == RuntimeState::Correct {
        plan.operations.push(SetupOperation::DeregisterRuntime);
        removals += 1;
    }
    for component in Component::ALL {
        if observed.component_state(component) != ComponentState::Missing {
            plan.operations
                .push(SetupOperation::RemoveManagedComponent { component });
            removals += 1;
        }
    }
    // Component detection asks "is this installed"; uninstall asks "does the
    // manifest still own anything". After a partial uninstall the answers
    // differ: nothing detects, files remain. The removal walks every owned
    // path whichever component it is issued for, so one is enough.
    if removals == 0 && observed.inventory_owned_paths > 0 {
        plan.operations
            .push(SetupOperation::RemoveManagedComponent {
                component: Component::Core,
            });
        removals += 1;
    }
    plan.operations.push(SetupOperation::RetainUserData);
    if removals == 0 {
        plan.outcomes.push(PlanOutcome::Noop {
            scope: "no manifest-owned setup wiring is present".to_string(),
        });
    }
    plan
}

pub trait SetupExecutor {
    type Error;

    fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error>;

    /// Removes only the completed steps for `component` from this execution.
    /// The default preserves simple executors that never create component-owned
    /// state, while the filesystem executor records each applied step by component.
    fn rollback_component(&mut self, _component: Component) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Whether the per-user daemon was live when this run observed the host.
    /// After core configuration is written, only a live daemon still holds the
    /// config it loaded at start and must be asked what a rescan would need.
    fn runtime_was_live(&self) -> bool {
        false
    }

    /// `supervisor.rescan { preview: true }` as `ck module rescan --dry-run`
    /// uses it. Returns the sections rescan cannot apply.
    fn preview_restart_required(&mut self) -> Result<Vec<String>, Self::Error> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    DryRun,
    Apply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupExecutionReport {
    pub planned: Vec<SetupOperation>,
    pub applied: Vec<SetupOperation>,
}

/// Executes only the mutation-bearing operations in an already planned,
/// authorized setup. A rejected plan and a dry run both leave the executor
/// untouched; validation and confirmation remain observations, not mutations.
pub fn execute_setup<E: SetupExecutor>(
    plan: &SetupPlan,
    mode: ExecutionMode,
    executor: &mut E,
) -> Result<SetupExecutionReport, E::Error> {
    let mut report = SetupExecutionReport {
        planned: plan.operations.clone(),
        applied: Vec::new(),
    };
    if mode == ExecutionMode::DryRun || !plan.is_authorized() {
        return Ok(report);
    }

    let mut pending_restart: Option<Vec<String>> = None;
    let mut restart_applied = false;
    for operation in plan
        .operations
        .iter()
        .filter(|operation| operation.mutates())
    {
        if matches!(
            operation,
            SetupOperation::RescanComponent { .. } | SetupOperation::EnableComponent { .. }
        ) && !restart_applied
        {
            if let Some(sections) = pending_restart.take() {
                apply_setup_operation(
                    executor,
                    &SetupOperation::RestartRuntime { sections },
                    &mut report,
                )?;
                restart_applied = true;
            }
        }
        apply_setup_operation(executor, operation, &mut report)?;
        if matches!(
            operation,
            SetupOperation::ConfigureComponent {
                component: Component::Core,
            }
        ) && executor.runtime_was_live()
        {
            // The write just happened. The live daemon still holds the config
            // it loaded at start; ask it whether rescan can apply the new file.
            let sections = executor.preview_restart_required()?;
            if !sections.is_empty() {
                pending_restart = Some(sections);
            }
        }
        if matches!(operation, SetupOperation::RestartRuntime { .. }) {
            restart_applied = true;
            pending_restart = None;
        }
    }
    if let Some(sections) = pending_restart {
        apply_setup_operation(
            executor,
            &SetupOperation::RestartRuntime { sections },
            &mut report,
        )?;
    }
    Ok(report)
}

fn apply_setup_operation<E: SetupExecutor>(
    executor: &mut E,
    operation: &SetupOperation,
    report: &mut SetupExecutionReport,
) -> Result<(), E::Error> {
    if let Err(error) = executor.apply(operation) {
        if let Some(component) = operation.component() {
            // Rollback is deliberately driven from the executor's record of
            // completed steps, rather than a compensating plan that could
            // accidentally remove state owned before this run.
            let _ = executor.rollback_component(component);
        }
        return Err(error);
    }
    report.applied.push(operation.clone());
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradePlan {
    pub operations: Vec<UpgradeOperation>,
    pub outcomes: Vec<PlanOutcome>,
}

impl UpgradePlan {
    pub fn is_authorized(&self) -> bool {
        !self.outcomes.iter().any(PlanOutcome::blocks_execution)
    }

    /// The dry-run text an operator reads before consenting.
    pub fn render(&self) -> String {
        let mut rendered = String::from("upgrade plan:\n");
        for (index, operation) in self.operations.iter().enumerate() {
            rendered.push_str(&format!("  {}. {operation}\n", index + 1));
        }
        for outcome in &self.outcomes {
            rendered.push_str(&format!("  outcome: {outcome}\n"));
        }
        rendered
    }
}

/// Plans the installed-component roster in ladder order, with self-replacement final.
pub fn plan_upgrade(observed: &UpgradeObserved) -> UpgradePlan {
    let mut plan = UpgradePlan {
        operations: vec![UpgradeOperation::ObservePlatform],
        outcomes: Vec::new(),
    };

    let PlatformObservation::Supported(_) = &observed.platform else {
        if let PlatformObservation::Unsupported(target) = &observed.platform {
            plan.outcomes.push(PlanOutcome::UnsupportedPlatform {
                target: target.clone(),
            });
        }
        return plan;
    };

    for &target in &observed.roster {
        match observed.target_state(target) {
            UpgradeState::NotInstalled => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{target} is not installed"),
            }),
            UpgradeState::Current => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{target} is already current"),
            }),
            UpgradeState::UpdateAvailable { from, to, reason } => {
                let release = observed.release(target);
                if release == ReleaseAvailability::Available {
                    if let Some(reason) = upgrade_core_floor_refusal(observed, target) {
                        plan.outcomes.push(PlanOutcome::TargetRefused {
                            component: target.component,
                            reason,
                        });
                        continue;
                    }
                }
                plan.outcomes.push(PlanOutcome::UpgradeAvailable {
                    target,
                    from,
                    to,
                    reason,
                });
                match release {
                    ReleaseAvailability::Incomplete { missing_asset } => {
                        plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                            component: target.component,
                            release_tag: "the resolved release".to_string(),
                            missing_asset,
                        });
                    }
                    ReleaseAvailability::NotYetPublished {
                        release_tag,
                        missing_asset,
                    } => {
                        plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                            component: target.component,
                            release_tag,
                            missing_asset,
                        });
                    }
                    ReleaseAvailability::Unresolvable { reason } => {
                        plan.outcomes.push(PlanOutcome::ReleaseUnresolvable {
                            component: target.component,
                            reason,
                        });
                    }
                    ReleaseAvailability::Available => {
                        if target.module_id().is_some()
                            && observed.daemon_unreachable_reason.is_some()
                        {
                            let reason = observed
                                .daemon_unreachable_reason
                                .as_deref()
                                .unwrap_or("daemon is unreachable");
                            plan.outcomes.push(PlanOutcome::Refusal {
                                reason: format!(
                                    "{target} requires a supervised restart, but the daemon is unreachable: {reason}"
                                ),
                            });
                        } else {
                            plan_upgrade_target(&mut plan, target, observed);
                        }
                    }
                    ReleaseAvailability::NotRequired => plan.outcomes.push(PlanOutcome::Refusal {
                        reason: format!("{target} has no alpha release archive"),
                    }),
                }
            }
        }
    }
    plan
}

fn upgrade_core_floor_refusal(observed: &UpgradeObserved, target: UpgradeTarget) -> Option<String> {
    let floor = observed.requires_core.get(&target.component)?;
    let required = match floor.parse::<CoreVersion>() {
        Ok(required) => required,
        Err(()) => {
            return Some(format!(
                "{} declares malformed requires_core `{floor}`",
                target.component
            ));
        }
    };
    let daemon_is_updating = observed.roster.iter().copied().any(|candidate| {
        candidate.is_daemon()
            && observed.release(candidate) == ReleaseAvailability::Available
            && matches!(
                observed.target_state(candidate),
                UpgradeState::UpdateAvailable { .. }
            )
    });
    let core_text = if daemon_is_updating {
        observed.available_core_version.as_deref()
    } else {
        observed.installed_core_version.as_deref()
    };
    let Some(core_text) = core_text else {
        return Some(format!(
            "{} requires core ≥ {floor}, but the core version the plan will leave running could not be read",
            target.component
        ));
    };
    let Ok(core_version) = core_text.parse::<CoreVersion>() else {
        return Some(format!(
            "{} requires core ≥ {floor}, but core version `{core_text}` could not be read",
            target.component
        ));
    };
    (core_version < required).then(|| {
        format!(
            "{} requires core ≥ {floor}, but the plan leaves core {core_text}",
            target.component
        )
    })
}

fn plan_upgrade_target(plan: &mut UpgradePlan, target: UpgradeTarget, observed: &UpgradeObserved) {
    plan.operations.extend([
        UpgradeOperation::DownloadAndVerify { target },
        UpgradeOperation::CreateRollbackCopy { target },
        UpgradeOperation::ReplaceDestination { target },
        UpgradeOperation::WarmExecute { target },
    ]);
    if target.module_id().is_some() {
        if observed.is_module_supervised(target) {
            plan.operations
                .push(UpgradeOperation::InitiateModuleRestart { target });
            plan.operations
                .push(UpgradeOperation::PollModuleRestartCompletion { target });
        } else {
            plan.outcomes
                .push(PlanOutcome::UnsupervisedModule { target });
        }
    } else if target.is_daemon() {
        plan.operations
            .push(UpgradeOperation::RestartDaemonViaServiceManager { target });
        plan.operations
            .push(UpgradeOperation::PollDaemonServiceReady { target });
    }
    plan.operations
        .push(UpgradeOperation::PostVerify { target });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::{
        components::upgrade_roster,
        model::{AlphaTarget, HostTarget, PlatformObservation},
    };
    use super::*;

    fn upgrade_target(binary: &str) -> UpgradeTarget {
        upgrade_roster(Component::ALL)
            .into_iter()
            .find(|target| target.label() == binary)
            .unwrap_or_else(|| panic!("missing upgrade target {binary}"))
    }

    #[derive(Default)]
    struct RecordingExecutor {
        applied: Vec<SetupOperation>,
    }

    impl SetupExecutor for RecordingExecutor {
        type Error = ();

        fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
            self.applied.push(operation.clone());
            Ok(())
        }
    }

    fn observed_setup() -> SetupObserved {
        let mut components = BTreeMap::new();
        components.insert(Component::Core, ComponentState::Missing);
        components.insert(Component::Aft, ComponentState::Correct);
        components.insert(Component::Mc, ComponentState::Missing);
        let mut releases = BTreeMap::new();
        for component in Component::ALL {
            releases.insert(component, ReleaseAvailability::Available);
        }
        SetupObserved {
            platform: PlatformObservation::Supported(AlphaTarget::LinuxX64),
            components,
            releases,
            requires_core: BTreeMap::new(),
            installed_core_version: None,
            runtime: RuntimeState::Missing,
            configuration: ConfigurationState::Additive,
            running_ck_adoption: None,
            mc_detection: None,
            detections: BTreeMap::new(),
            restart_required: Vec::new(),
            inventory_owned_paths: 0,
        }
    }

    fn installed_core_setup(installed_version: Option<&str>) -> SetupObserved {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Aft, ComponentState::Missing);
        observed
            .requires_core
            .insert(Component::Aft, "0.17.20".to_string());
        observed.installed_core_version = installed_version.map(ToOwned::to_owned);
        observed.runtime = RuntimeState::Correct;
        observed
    }

    fn updates_target(plan: &UpgradePlan, target: UpgradeTarget) -> bool {
        plan.operations.iter().any(|operation| {
            matches!(
                operation,
                UpgradeOperation::DownloadAndVerify { target: planned } if *planned == target
            )
        })
    }

    fn floor_upgrade_observed(
        floor: &str,
        planned_core: Option<&str>,
        installed_core: Option<&str>,
    ) -> UpgradeObserved {
        let mut observed = UpgradeObserved::for_roster(upgrade_roster([
            Component::Core,
            Component::Aft,
            Component::Insula,
        ]));
        observed.platform = PlatformObservation::Supported(AlphaTarget::LinuxX64);
        observed
            .requires_core
            .insert(Component::Aft, floor.to_string());
        observed.available_core_version = planned_core.map(ToOwned::to_owned);
        observed.installed_core_version = installed_core.map(ToOwned::to_owned);
        observed.supervised_modules.extend([
            "aft".to_string(),
            "insula".to_string(),
            "subc-mcp".to_string(),
        ]);
        for binary in ["ck-aft", "ck-insula"] {
            observed.targets.insert(
                binary.to_string(),
                UpgradeState::UpdateAvailable {
                    from: "0.17.0".to_string(),
                    to: "0.18.0".to_string(),
                    reason: None,
                },
            );
        }
        if let Some(version) = planned_core {
            observed.targets.insert(
                "ck-subc".to_string(),
                UpgradeState::UpdateAvailable {
                    from: installed_core.unwrap_or("0.17.0").to_string(),
                    to: version.to_string(),
                    reason: None,
                },
            );
        }
        observed
    }

    #[test]
    fn dry_run_and_apply_use_the_same_plan_but_only_apply_mutations() {
        let mut observed = observed_setup();
        observed
            .requires_core
            .insert(Component::Mc, "0.17.20".to_string());
        let request = SetupRequest::install(vec![Component::Mc]);
        let preview_plan = plan_setup(&observed, &request);
        let execution_plan = plan_setup(&observed, &request);
        assert_eq!(preview_plan, execution_plan);
        assert_ne!(preview_plan.mutation_count(), 0);

        let mut preview_executor = RecordingExecutor::default();
        let preview =
            execute_setup(&preview_plan, ExecutionMode::DryRun, &mut preview_executor).unwrap();
        assert_eq!(preview.planned, execution_plan.operations);
        assert!(preview.applied.is_empty());
        assert!(preview_executor.applied.is_empty());

        let mut apply_executor = RecordingExecutor::default();
        let applied =
            execute_setup(&execution_plan, ExecutionMode::Apply, &mut apply_executor).unwrap();
        let expected_mutations = execution_plan
            .operations
            .iter()
            .filter(|operation| operation.mutates())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(applied.planned, preview.planned);
        assert_eq!(applied.applied, expected_mutations);
        assert_eq!(apply_executor.applied, expected_mutations);
    }

    #[test]
    fn setup_refuses_a_module_below_its_floor_and_dry_run_matches_apply() {
        let observed = installed_core_setup(Some("0.17.19"));
        let request = SetupRequest::install(vec![Component::Aft]);
        let preview_plan = plan_setup(&observed, &request);
        let execution_plan = plan_setup(&observed, &request);

        assert_eq!(preview_plan, execution_plan);
        assert!(preview_plan.is_authorized());
        assert!(!preview_plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::InstallComponent {
                component: Component::Aft
            }
        )));
        assert!(preview_plan.outcomes.iter().any(|outcome| {
            outcome.to_string()
                == "refusal: aft requires core ≥ 0.17.20, installed 0.17.19; run `ck upgrade` first"
        }));

        let mut preview_executor = RecordingExecutor::default();
        let preview =
            execute_setup(&preview_plan, ExecutionMode::DryRun, &mut preview_executor).unwrap();
        let mut apply_executor = RecordingExecutor::default();
        let applied =
            execute_setup(&execution_plan, ExecutionMode::Apply, &mut apply_executor).unwrap();
        assert_eq!(preview.planned, applied.planned);
        assert!(preview.applied.is_empty());
        assert!(applied.applied.is_empty());
        assert!(apply_executor.applied.is_empty());
    }

    #[test]
    fn setup_installs_a_module_when_installed_core_meets_its_floor() {
        for installed in ["0.17.20", "0.18.0"] {
            let observed = installed_core_setup(Some(installed));
            let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
            assert!(plan.is_authorized(), "{installed}: {:?}", plan.outcomes);
            assert!(plan.operations.iter().any(|operation| matches!(
                operation,
                SetupOperation::InstallComponent {
                    component: Component::Aft
                }
            )));
        }
    }

    #[test]
    fn fresh_setup_installs_core_before_a_floored_module_without_a_refusal() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Aft, ComponentState::Missing);
        observed
            .requires_core
            .insert(Component::Aft, "0.17.20".to_string());
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
        let core = plan
            .operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    SetupOperation::InstallComponent {
                        component: Component::Core
                    }
                )
            })
            .expect("core install");
        let aft = plan
            .operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    SetupOperation::InstallComponent {
                        component: Component::Aft
                    }
                )
            })
            .expect("aft install");
        assert!(core < aft);
        assert!(!plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::Refusal { .. } | PlanOutcome::TargetRefused { .. }
        )));
    }

    #[test]
    fn setup_refuses_a_floored_module_when_daemon_version_is_unreadable() {
        let observed = installed_core_setup(None);
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
        assert!(!plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::InstallComponent {
                component: Component::Aft
            }
        )));
        assert!(plan.outcomes.iter().any(|outcome| {
            outcome.to_string().contains(
                "aft requires core ≥ 0.17.20, but the installed core version could not be read",
            )
        }));
    }

    #[test]
    fn correct_aft_is_not_disturbed_when_mc_is_added() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed.runtime = RuntimeState::Correct;
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Mc]));
        assert!(plan.operations.iter().any(|operation| {
            matches!(
                operation,
                SetupOperation::InstallComponent {
                    component: Component::Mc
                }
            )
        }));
        assert!(!plan.operations.iter().any(|operation| {
            matches!(
                operation,
                SetupOperation::ConfigureComponent {
                    component: Component::Aft
                }
            )
        }));
        assert!(!plan.operations.iter().any(|operation| {
            matches!(
                operation,
                SetupOperation::InstallComponent {
                    component: Component::Core
                } | SetupOperation::ConfigureComponent {
                    component: Component::Core
                } | SetupOperation::RegisterRuntime
                    | SetupOperation::StartRuntime
            )
        }));
    }

    #[test]
    fn refusal_and_owner_gate_are_distinct_and_apply_nothing() {
        let mut observed = observed_setup();
        observed.configuration = ConfigurationState::Conflict {
            key: "transport".to_string(),
        };
        observed.detections.insert(
            Component::Aft,
            DetectionOutcome::OwnerGated {
                reason: "detector contract absent".to_string(),
            },
        );
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
        assert!(plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::Refusal { reason } if reason.contains("transport")
        )));
        assert!(plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::OwnerGatedDetection {
                component: Component::Aft,
                ..
            }
        )));
        let mut executor = RecordingExecutor::default();
        let report = execute_setup(&plan, ExecutionMode::Apply, &mut executor).unwrap();
        assert!(report.applied.is_empty());
        assert!(executor.applied.is_empty());
    }

    /// After an uninstall that failed partway, no component detects (the
    /// daemon binary and config rows are gone) but the manifest still owns
    /// files on disk. The retry must plan a removal for them, and an empty
    /// manifest must still plan nothing.
    #[test]
    fn uninstall_plans_a_removal_for_manifest_residue_no_component_detects() {
        let request = SetupRequest {
            uninstall: true,
            ..SetupRequest::install(Vec::new())
        };
        let mut residue = observed_setup();
        for component in Component::ALL {
            residue
                .components
                .insert(component, ComponentState::Missing);
        }
        residue.runtime = RuntimeState::Missing;
        residue.inventory_owned_paths = 2;
        let plan = plan_setup(&residue, &request);
        assert!(
            plan.operations
                .iter()
                .any(|op| matches!(op, SetupOperation::RemoveManagedComponent { .. })),
            "{:?}",
            plan.operations
        );
        assert!(!plan
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, PlanOutcome::Noop { .. })));

        residue.inventory_owned_paths = 0;
        let plan = plan_setup(&residue, &request);
        assert!(!plan
            .operations
            .iter()
            .any(|op| matches!(op, SetupOperation::RemoveManagedComponent { .. })));
        assert!(plan
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, PlanOutcome::Noop { .. })));
    }

    #[test]
    fn unsupported_and_release_incomplete_are_not_collapsed_into_refusals() {
        let unsupported = SetupObserved {
            platform: PlatformObservation::Unsupported(HostTarget {
                os: "freebsd".to_string(),
                arch: "x64".to_string(),
            }),
            ..observed_setup()
        };
        let unsupported_plan = plan_setup(&unsupported, &SetupRequest::install(Vec::new()));
        assert!(matches!(
            unsupported_plan.outcomes.as_slice(),
            [PlanOutcome::UnsupportedPlatform { .. }]
        ));

        let mut incomplete = observed_setup();
        incomplete.releases.insert(
            Component::Aft,
            ReleaseAvailability::NotYetPublished {
                release_tag: "v0.1.0".to_string(),
                missing_asset: "ck-aft-linux-x64.zip".to_string(),
            },
        );
        let incomplete_plan = plan_setup(&incomplete, &SetupRequest::install(vec![Component::Aft]));
        let outcome = incomplete_plan
            .outcomes
            .iter()
            .find(|outcome| matches!(outcome, PlanOutcome::ReleaseIncomplete { .. }))
            .expect("typed temporal release outcome");
        assert_eq!(
            outcome.to_string(),
            "aft: no ck-aft-linux-x64.zip asset in v0.1.0 yet — the module's owner has not published this platform"
        );
    }

    /// Fifth finding of the macOS operator drive: a 403 from the release API
    /// (the unauthenticated rate limit, exhausted by earlier drives) was
    /// rendered as "the module's owner has not published this platform" — a
    /// false statement about the owner made from a fact about the request.
    /// An unresolvable release names what happened, blocks that component
    /// only, and never claims anything about publication.
    #[test]
    fn unresolvable_release_is_reported_as_the_request_failure_not_as_unpublished() {
        let mut observed = observed_setup();
        observed.releases.insert(
            Component::Aft,
            ReleaseAvailability::Unresolvable {
                reason: "index_unreachable: https://cortexkit.io/releases/v1/index.json: HTTP 503"
                    .to_string(),
            },
        );
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
        let outcome = plan
            .outcomes
            .iter()
            .find(|outcome| matches!(outcome, PlanOutcome::ReleaseUnresolvable { .. }))
            .expect("typed unresolvable outcome");
        let rendered = outcome.to_string();
        assert!(
            rendered.contains("could not resolve the release"),
            "{rendered}"
        );
        assert!(rendered.contains("index_unreachable"), "{rendered}");
        assert!(!rendered.contains("has not published"), "{rendered}");
        assert!(outcome.blocks_execution());
        // Core was resolvable and stays plannable: one component's host
        // failure must not silence the others.
        assert!(!plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::ReleaseUnresolvable {
                component: Component::Core,
                ..
            }
        )));
    }

    #[test]
    fn automatic_mc_offer_does_not_install_or_configure_mc() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed.runtime = RuntimeState::Correct;
        observed
            .detections
            .insert(Component::Mc, DetectionOutcome::OfferConversion);

        let plan = plan_setup(&observed, &SetupRequest::install(Vec::new()));

        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::OfferConversion {
                component: Component::Mc
            }
        )));
        assert!(!plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::InstallComponent {
                component: Component::Mc
            } | SetupOperation::ConfigureComponent {
                component: Component::Mc
            }
        )));
        let mut executor = RecordingExecutor::default();
        let report = execute_setup(&plan, ExecutionMode::Apply, &mut executor).unwrap();
        assert!(report.applied.is_empty());
        assert!(executor.applied.is_empty());
    }

    #[test]
    fn confirmed_mc_conversion_reuses_component_addition_and_declining_applies_nothing() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed.runtime = RuntimeState::Correct;

        let mut accepted = SetupRequest::install(Vec::new());
        accepted.convert = Some(Component::Mc);
        accepted.conversion_confirmed = true;
        let accepted_plan = plan_setup(&observed, &accepted);
        assert!(accepted_plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::ConfirmConversion {
                component: Component::Mc
            }
        )));
        assert!(accepted_plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::InstallComponent {
                component: Component::Mc
            }
        )));
        assert!(accepted_plan.operations.iter().any(|operation| matches!(
            operation,
            SetupOperation::ConfigureComponent {
                component: Component::Mc
            }
        )));

        let mut declined = accepted.clone();
        declined.conversion_confirmed = false;
        let declined_plan = plan_setup(&observed, &declined);
        assert!(!declined_plan.is_authorized());
        let mut executor = RecordingExecutor::default();
        let report = execute_setup(&declined_plan, ExecutionMode::Apply, &mut executor).unwrap();
        assert!(report.applied.is_empty());
        assert!(executor.applied.is_empty());
    }

    #[test]
    fn claustrum_bootstraps_before_it_is_enabled_with_one_key_path() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed.runtime = RuntimeState::Correct;
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Missing);
        let key_path = std::path::PathBuf::from("/keys/claustrum.key");
        let mut request = SetupRequest::install(vec![Component::Claustrum]);
        request.claustrum_key_path = Some(key_path.clone());
        let operations = plan_setup(&observed, &request).operations;
        let bootstrap = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    SetupOperation::BootstrapClaustrum { key_path: Some(path), .. } if path == &key_path
                )
            })
            .expect("bootstrap operation");
        let enable = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    SetupOperation::EnableComponent {
                        component: Component::Claustrum
                    }
                )
            })
            .expect("enable operation");
        assert!(bootstrap < enable);
    }

    #[derive(Default)]
    struct FailingBootstrapExecutor {
        managed_binary_rows: BTreeMap<Component, usize>,
        config_entries: BTreeMap<Component, usize>,
        rolled_back: Vec<Component>,
        bootstrap_calls: usize,
        bootstrap_succeeds: bool,
        applied: Vec<SetupOperation>,
    }

    impl SetupExecutor for FailingBootstrapExecutor {
        type Error = String;

        fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
            match operation {
                SetupOperation::InstallComponent { component } => {
                    self.managed_binary_rows.insert(*component, 1);
                    self.applied.push(operation.clone());
                    Ok(())
                }
                SetupOperation::ConfigureComponent { component } => {
                    self.config_entries.insert(*component, 1);
                    self.applied.push(operation.clone());
                    Ok(())
                }
                SetupOperation::BootstrapClaustrum { .. } => {
                    self.bootstrap_calls += 1;
                    if self.bootstrap_succeeds {
                        self.applied.push(operation.clone());
                        Ok(())
                    } else {
                        Err(
                            "key store is not writable; ck auth bootstrap failed with exit status: 4"
                                .to_string(),
                        )
                    }
                }
                _ => {
                    self.applied.push(operation.clone());
                    Ok(())
                }
            }
        }

        fn rollback_component(&mut self, component: Component) -> Result<(), Self::Error> {
            self.managed_binary_rows.remove(&component);
            self.config_entries.remove(&component);
            self.rolled_back.push(component);
            Ok(())
        }
    }

    #[test]
    fn failed_claustrum_bootstrap_rolls_back_only_its_own_apply_steps() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Aft, ComponentState::Missing);
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Missing);
        observed.runtime = RuntimeState::Correct;
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Aft, Component::Claustrum]),
        );
        let mut executor = FailingBootstrapExecutor::default();

        let error = execute_setup(&plan, ExecutionMode::Apply, &mut executor)
            .expect_err("bootstrap refusal must fail setup");

        assert_eq!(
            error,
            "key store is not writable; ck auth bootstrap failed with exit status: 4"
        );
        assert!(
            !executor
                .managed_binary_rows
                .contains_key(&Component::Claustrum),
            "claustrum binary inventory rows must be gone"
        );
        assert!(
            !executor.config_entries.contains_key(&Component::Claustrum),
            "claustrum configuration entry must be gone"
        );
        assert_eq!(
            executor.managed_binary_rows.get(&Component::Aft),
            Some(&1),
            "sibling binary inventory row must survive"
        );
        assert_eq!(
            executor.config_entries.get(&Component::Aft),
            Some(&1),
            "sibling configuration entry must survive"
        );
        assert_eq!(executor.rolled_back, [Component::Claustrum]);
        assert_eq!(executor.bootstrap_calls, 1);
    }

    #[test]
    fn configured_module_plans_only_registration_operations() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Aft, ComponentState::Configured);
        observed.runtime = RuntimeState::Correct;

        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Aft]));
        let component_operations = plan
            .operations
            .iter()
            .filter(|operation| operation.component().is_some())
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            component_operations,
            vec![
                SetupOperation::RescanComponent {
                    component: Component::Aft,
                },
                SetupOperation::EnableComponent {
                    component: Component::Aft,
                },
            ]
        );
        assert!(plan.outcomes.iter().any(|outcome| {
            outcome.to_string() == "aft: configured but not registered; registering"
        }));
    }

    #[test]
    fn configured_claustrum_bootstraps_before_rescan_and_enable() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Configured);
        observed.runtime = RuntimeState::Correct;

        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        let mut executor = FailingBootstrapExecutor {
            bootstrap_succeeds: true,
            ..Default::default()
        };
        execute_setup(&plan, ExecutionMode::Apply, &mut executor)
            .expect("idempotent configured bootstrap must succeed");

        assert_eq!(executor.bootstrap_calls, 1);
        assert_eq!(
            executor.applied,
            vec![
                SetupOperation::BootstrapClaustrum { key_path: None },
                SetupOperation::RescanComponent {
                    component: Component::Claustrum,
                },
                SetupOperation::EnableComponent {
                    component: Component::Claustrum,
                },
            ]
        );
        assert!(plan.outcomes.iter().any(|outcome| {
            outcome.to_string() == "claustrum: configured but not registered; registering"
        }));
    }

    #[test]
    fn configured_claustrum_bootstrap_failure_does_not_enable_the_module() {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Configured);
        observed.runtime = RuntimeState::Correct;
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        let mut executor = FailingBootstrapExecutor::default();

        let error = execute_setup(&plan, ExecutionMode::Apply, &mut executor)
            .expect_err("non-zero bootstrap must fail configured repair");

        assert_eq!(executor.bootstrap_calls, 1);
        assert_eq!(
            error,
            "key store is not writable; ck auth bootstrap failed with exit status: 4"
        );
        assert!(!executor.applied.iter().any(|operation| {
            matches!(
                operation,
                SetupOperation::EnableComponent {
                    component: Component::Claustrum
                }
            )
        }));
    }

    #[test]
    fn mc_windows_is_stated_as_declared_unavailable() {
        let mut observed = observed_setup();
        observed.platform = PlatformObservation::Supported(AlphaTarget::WindowsX64);
        let plan = plan_setup(&observed, &SetupRequest::install(vec![Component::Mc]));
        assert!(plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::DeclaredUnavailable { message, .. }
                if message == "magic-context: not available on windows in alpha"
        )));
        assert!(plan.is_authorized());
    }

    #[test]
    fn upgrade_allows_a_floored_module_when_planned_core_meets_the_floor() {
        let observed = floor_upgrade_observed("0.17.20", Some("0.17.20"), Some("0.17.19"));
        let plan = plan_upgrade(&observed);
        assert!(updates_target(&plan, upgrade_target("ck-subc")));
        assert!(updates_target(&plan, upgrade_target("ck-aft")));
        assert!(!plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::TargetRefused { reason, .. } if reason.contains("aft")
        )));
    }

    #[test]
    fn upgrade_refuses_only_the_module_above_the_planned_core_floor() {
        let observed = floor_upgrade_observed("0.18.0", Some("0.17.34"), Some("0.17.20"));
        let plan = plan_upgrade(&observed);
        assert!(updates_target(&plan, upgrade_target("ck-subc")));
        assert!(updates_target(&plan, upgrade_target("ck-insula")));
        assert!(!updates_target(&plan, upgrade_target("ck-aft")));
        assert!(plan.is_authorized(), "other targets must remain executable");
        assert!(plan.outcomes.iter().any(|outcome| {
            outcome.to_string()
                == "refusal: aft requires core ≥ 0.18.0, but the plan leaves core 0.17.34"
        }));
    }

    #[test]
    fn release_incomplete_still_blocks_beside_a_floor_refused_module() {
        let mut observed = floor_upgrade_observed("0.18.0", Some("0.17.34"), Some("0.17.20"));
        observed.releases.insert(
            "ck-insula".to_string(),
            ReleaseAvailability::Incomplete {
                missing_asset: "ck-insula-linux-x64.zip".to_string(),
            },
        );
        let plan = plan_upgrade(&observed);

        assert!(plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::TargetRefused {
                component: Component::Aft,
                ..
            }
        )));
        assert!(plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::ReleaseIncomplete {
                component: Component::Insula,
                ..
            }
        )));
        assert!(!plan.is_authorized());
    }

    #[test]
    fn upgrade_without_a_core_update_uses_the_installed_daemon_version() {
        let below = floor_upgrade_observed("0.17.20", None, Some("0.17.19"));
        let below_plan = plan_upgrade(&below);
        assert!(!updates_target(&below_plan, upgrade_target("ck-aft")));
        assert!(below_plan.outcomes.iter().any(|outcome| {
            outcome
                .to_string()
                .contains("aft requires core ≥ 0.17.20, but the plan leaves core 0.17.19")
        }));

        let meeting = floor_upgrade_observed("0.17.20", None, Some("0.17.20"));
        let meeting_plan = plan_upgrade(&meeting);
        assert!(updates_target(&meeting_plan, upgrade_target("ck-aft")));
        assert!(!meeting_plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            PlanOutcome::TargetRefused { reason, .. } if reason.contains("aft")
        )));
    }

    #[test]
    fn upgrade_refuses_a_malformed_module_floor_and_names_its_value() {
        let observed = floor_upgrade_observed("v0.17.20", Some("0.18.0"), Some("0.17.20"));
        let plan = plan_upgrade(&observed);
        assert!(!updates_target(&plan, upgrade_target("ck-aft")));
        assert!(plan.outcomes.iter().any(|outcome| {
            outcome.to_string() == "refusal: aft declares malformed requires_core `v0.17.20`"
        }));
    }

    #[test]
    fn upgrade_orders_daemon_service_then_module_ack_poll_then_ck_without_mc() {
        let roster = upgrade_roster([Component::Core, Component::Aft]);
        let mut targets = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for target in &roster {
            targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "1.1.0".to_string(),
                    reason: None,
                },
            );
            releases.insert(target.label().to_string(), ReleaseAvailability::Available);
        }
        let mut supervised_modules = std::collections::BTreeSet::new();
        supervised_modules.insert("aft".to_string());
        supervised_modules.insert("subc-mcp".to_string());
        let plan = plan_upgrade(&UpgradeObserved {
            platform: PlatformObservation::Supported(AlphaTarget::LinuxX64),
            roster,
            targets,
            releases,
            requires_core: BTreeMap::new(),
            available_core_version: None,
            installed_core_version: Some("1.0.0".to_string()),
            supervised_modules,
            daemon_unreachable_reason: None,
        });
        assert!(!plan
            .operations
            .iter()
            .any(|operation| operation.to_string().contains("ck-mc")));

        let aft = upgrade_target("ck-aft");
        let ck = upgrade_target("ck");
        let operations = &plan.operations;
        let aft_poll = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    UpgradeOperation::PollModuleRestartCompletion { target } if *target == aft
                )
            })
            .unwrap();
        let daemon_restart = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    UpgradeOperation::RestartDaemonViaServiceManager { .. }
                )
            })
            .unwrap();
        let ck_replace = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    UpgradeOperation::ReplaceDestination { target } if *target == ck
                )
            })
            .unwrap();
        // Daemon before modules: a module rebuilt against a newer wire crate
        // must restart into a daemon that already accepts its HELLO.
        assert!(daemon_restart < aft_poll);
        assert!(aft_poll < ck_replace);
        assert!(!operations.iter().any(|operation| matches!(
            operation,
            UpgradeOperation::InitiateModuleRestart { target }
                | UpgradeOperation::PollModuleRestartCompletion { target }
                if *target == ck
        )));
    }

    fn live_runtime_adding_claustrum() -> SetupObserved {
        let mut observed = observed_setup();
        observed
            .components
            .insert(Component::Core, ComponentState::Correct);
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Missing);
        observed.runtime = RuntimeState::Correct;
        observed
    }

    /// Dropping this ordering (restart after rescan) must fail this test by
    /// name: a rescan against a daemon that still holds its start-time config
    /// is the crash-loop this step exists to prevent.
    #[test]
    fn restart_runtime_is_planned_before_the_first_rescan() {
        let sections =
            super::super::config::restart_required_from_pending_keys(true, ["storage.backend"]);
        assert_eq!(sections, ["storage"]);
        let mut observed = live_runtime_adding_claustrum();
        observed.restart_required = sections;
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        let restarts = plan
            .operations
            .iter()
            .filter(|operation| matches!(operation, SetupOperation::RestartRuntime { .. }))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            restarts,
            vec![SetupOperation::RestartRuntime {
                sections: vec!["storage".to_string()],
            }]
        );
        let restart_pos = plan
            .operations
            .iter()
            .position(|operation| matches!(operation, SetupOperation::RestartRuntime { .. }))
            .expect("RestartRuntime");
        let rescan_pos = plan
            .operations
            .iter()
            .position(|operation| matches!(operation, SetupOperation::RescanComponent { .. }))
            .expect("RescanComponent");
        assert!(
            restart_pos < rescan_pos,
            "RestartRuntime must precede the first RescanComponent; restart_pos={restart_pos} rescan_pos={rescan_pos}"
        );
    }

    #[test]
    fn live_host_missing_storage_plans_restart_with_zero_preview_calls() {
        let sections =
            super::super::config::restart_required_from_pending_keys(true, ["storage.backend"]);
        assert_eq!(sections, ["storage"]);
        let mut observed = live_runtime_adding_claustrum();
        observed.restart_required = sections;
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        assert_eq!(
            plan.operations
                .iter()
                .filter(|operation| matches!(operation, SetupOperation::RestartRuntime { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn live_host_already_carrying_storage_plans_no_restart_runtime() {
        let sections =
            super::super::config::restart_required_from_pending_keys(true, [] as [&str; 0]);
        assert!(sections.is_empty());
        let mut observed = live_runtime_adding_claustrum();
        observed.restart_required = sections;
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        assert!(!plan
            .operations
            .iter()
            .any(|operation| matches!(operation, SetupOperation::RestartRuntime { .. })));
        assert!(!plan
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, PlanOutcome::CoreRestartRequired { .. })));
    }

    #[test]
    fn runtime_not_live_plans_no_restart_even_when_storage_would_be_written() {
        let sections =
            super::super::config::restart_required_from_pending_keys(false, ["storage.backend"]);
        assert!(sections.is_empty());
    }

    #[test]
    fn dry_run_plan_rendering_carries_the_restart_section_name() {
        assert_eq!(
            SetupOperation::RestartRuntime {
                sections: vec!["storage".to_string()],
            }
            .to_string(),
            "restart daemon: config sections changed that rescan cannot apply: storage"
        );
        assert_eq!(
            PlanOutcome::CoreRestartRequired {
                sections: vec!["storage".to_string()],
            }
            .to_string(),
            "core: configuration change requires a daemon restart (storage)"
        );
        let mut observed = live_runtime_adding_claustrum();
        observed.restart_required = vec!["storage".to_string()];
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        let rendered = plan.render();
        assert!(
            rendered.contains(
                "restart daemon: config sections changed that rescan cannot apply: storage"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "outcome: core: configuration change requires a daemon restart (storage)"
            ),
            "{rendered}"
        );
    }

    #[derive(Default)]
    struct LivePreviewExecutor {
        applied: Vec<SetupOperation>,
        preview_calls: usize,
        preview_result: Vec<String>,
        live: bool,
    }

    impl SetupExecutor for LivePreviewExecutor {
        type Error = String;

        fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
            self.applied.push(operation.clone());
            Ok(())
        }

        fn runtime_was_live(&self) -> bool {
            self.live
        }

        fn preview_restart_required(&mut self) -> Result<Vec<String>, Self::Error> {
            self.preview_calls += 1;
            Ok(self.preview_result.clone())
        }
    }

    #[test]
    fn after_core_configure_a_live_preview_injects_restart_before_rescan() {
        let mut observed = observed_setup();
        observed.runtime = RuntimeState::Correct;
        observed
            .components
            .insert(Component::Claustrum, ComponentState::Missing);
        let plan = plan_setup(
            &observed,
            &SetupRequest::install(vec![Component::Claustrum]),
        );
        assert!(!plan
            .operations
            .iter()
            .any(|operation| matches!(operation, SetupOperation::RestartRuntime { .. })));
        let mut executor = LivePreviewExecutor {
            live: true,
            preview_result: vec!["storage".to_string()],
            ..Default::default()
        };
        let report = execute_setup(&plan, ExecutionMode::Apply, &mut executor).unwrap();
        assert_eq!(executor.preview_calls, 1);
        let restart_pos = report
            .applied
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    SetupOperation::RestartRuntime { sections }
                        if sections == &["storage".to_string()]
                )
            })
            .expect("injected RestartRuntime");
        let rescan_pos = report
            .applied
            .iter()
            .position(|operation| matches!(operation, SetupOperation::RescanComponent { .. }))
            .expect("RescanComponent");
        assert!(restart_pos < rescan_pos);
        assert_eq!(
            report
                .applied
                .iter()
                .filter(|operation| matches!(operation, SetupOperation::RestartRuntime { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn unreachable_daemon_with_aft_update_emits_typed_refusal_and_plans_non_module_targets() {
        let mut observed =
            UpgradeObserved::for_roster(upgrade_roster([Component::Core, Component::Aft]));
        observed.daemon_unreachable_reason = Some("daemon connection refused".to_string());
        let aft = upgrade_target("ck-aft");
        let ck = upgrade_target("ck");
        let daemon = upgrade_target("ck-subc");
        for target in [aft, ck, daemon] {
            observed.targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                    reason: None,
                },
            );
        }

        let plan = plan_upgrade(&observed);

        let aft_refusal = plan.outcomes.iter().find(|outcome| {
            matches!(
                outcome,
                PlanOutcome::Refusal { reason } if reason.contains("ck-aft")
            )
        });
        assert!(
            aft_refusal.is_some(),
            "expected typed refusal on aft: {:?}",
            plan.outcomes
        );

        assert!(!plan.operations.iter().any(|operation| matches!(
            operation,
            UpgradeOperation::InitiateModuleRestart { target }
                | UpgradeOperation::PollModuleRestartCompletion { target }
                if *target == aft
        )));

        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            UpgradeOperation::ReplaceDestination { target } if *target == ck
        )));
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            UpgradeOperation::RestartDaemonViaServiceManager { target } if *target == daemon
        )));
    }

    #[test]
    fn dry_run_renders_unsupervised_module_reason_in_one_line_and_omits_restart() {
        let mut observed =
            UpgradeObserved::for_roster(upgrade_roster([Component::Core, Component::Aft]));
        observed.supervised_modules.insert("aft".to_string());
        let subc_mcp = upgrade_target("ck-subc-mcp");
        observed.targets.insert(
            subc_mcp.label().to_string(),
            UpgradeState::UpdateAvailable {
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
                reason: None,
            },
        );
        let plan = plan_upgrade(&observed);
        let rendered = plan.render();

        assert!(!rendered.contains("initiate supervised restart for ck-subc-mcp"));
        assert!(!rendered.contains("poll supervised restart completion for ck-subc-mcp"));
        assert!(rendered.contains(
            "outcome: ck-subc-mcp: module is not supervised on this host; restart omitted, verified by binary version only"
        ));
    }
}
