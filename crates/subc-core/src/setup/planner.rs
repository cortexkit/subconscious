use super::{
    conversion::{explicit_conversion_requires_confirmation, selected_components},
    model::{
        Component, ComponentState, ConfigurationState, DetectionOutcome, PlanOutcome,
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
            ReleaseAvailability::Available | ReleaseAvailability::NotRequired => {}
        }

        match observed.component_state(component) {
            ComponentState::Correct => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{component} is already correct"),
            }),
            ComponentState::Missing => {
                plan.operations
                    .push(SetupOperation::InstallComponent { component });
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
        SetupOperation::Validate {
            instrument: "ck fleet lint",
        },
    ]);
    plan
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
        if observed.component_state(component) == ComponentState::Correct {
            plan.operations
                .push(SetupOperation::RemoveManagedComponent { component });
            removals += 1;
        }
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

    for operation in plan
        .operations
        .iter()
        .filter(|operation| operation.mutates())
    {
        executor.apply(operation)?;
        report.applied.push(operation.clone());
    }
    Ok(report)
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
}

/// Plans modules before the daemon and always makes the self-replacement final.
/// The plan purposefully has no MC target because MC has no alpha archive.
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

    for target in UpgradeTarget::ORDERED {
        match observed.target_state(target) {
            UpgradeState::NotInstalled => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{target} is not installed"),
            }),
            UpgradeState::Current => plan.outcomes.push(PlanOutcome::Noop {
                scope: format!("{target} is already current"),
            }),
            UpgradeState::UpdateAvailable { .. } => match observed.release(target) {
                ReleaseAvailability::Incomplete { missing_asset } => {
                    plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                        component: upgrade_target_component(target),
                        release_tag: "the resolved release".to_string(),
                        missing_asset,
                    });
                }
                ReleaseAvailability::NotYetPublished {
                    release_tag,
                    missing_asset,
                } => {
                    plan.outcomes.push(PlanOutcome::ReleaseIncomplete {
                        component: upgrade_target_component(target),
                        release_tag,
                        missing_asset,
                    });
                }
                ReleaseAvailability::Available => plan_upgrade_target(&mut plan, target),
                ReleaseAvailability::NotRequired => plan.outcomes.push(PlanOutcome::Refusal {
                    reason: format!("{target} has no alpha release archive"),
                }),
            },
        }
    }
    plan
}

fn plan_upgrade_target(plan: &mut UpgradePlan, target: UpgradeTarget) {
    plan.operations.extend([
        UpgradeOperation::DownloadAndVerify { target },
        UpgradeOperation::CreateRollbackCopy { target },
        UpgradeOperation::ReplaceDestination { target },
        UpgradeOperation::WarmExecute { target },
    ]);
    match target {
        UpgradeTarget::SubcMcp | UpgradeTarget::Aft => {
            plan.operations
                .push(UpgradeOperation::InitiateModuleRestart { target });
            plan.operations
                .push(UpgradeOperation::PollModuleRestartCompletion { target });
        }
        UpgradeTarget::Daemon => {
            plan.operations
                .push(UpgradeOperation::RestartDaemonViaServiceManager);
            plan.operations
                .push(UpgradeOperation::PollDaemonServiceReady);
        }
        UpgradeTarget::Ck => {}
    }
    plan.operations
        .push(UpgradeOperation::PostVerify { target });
}

fn upgrade_target_component(target: UpgradeTarget) -> Component {
    match target {
        UpgradeTarget::Aft => Component::Aft,
        UpgradeTarget::SubcMcp | UpgradeTarget::Daemon | UpgradeTarget::Ck => Component::Core,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::model::{AlphaTarget, HostTarget, PlatformObservation};
    use super::*;

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
            runtime: RuntimeState::Missing,
            configuration: ConfigurationState::Additive,
            mc_detection: None,
            detections: BTreeMap::new(),
        }
    }

    #[test]
    fn dry_run_and_apply_use_the_same_plan_but_only_apply_mutations() {
        let observed = observed_setup();
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
                missing_asset: "aft-linux-x64.zip".to_string(),
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
            "aft: no aft-linux-x64.zip asset in v0.1.0 yet — the module's owner has not published this platform"
        );
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
                    SetupOperation::BootstrapClaustrum { key_path: Some(path) } if path == &key_path
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
    fn upgrade_orders_module_ack_poll_then_daemon_service_then_ck_without_mc() {
        let mut targets = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for target in UpgradeTarget::ORDERED {
            targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "1.1.0".to_string(),
                },
            );
            releases.insert(target.label().to_string(), ReleaseAvailability::Available);
        }
        let plan = plan_upgrade(&UpgradeObserved {
            platform: PlatformObservation::Supported(AlphaTarget::LinuxX64),
            targets,
            releases,
        });
        assert!(!plan
            .operations
            .iter()
            .any(|operation| operation.to_string().contains("ck-mc")));

        let operations = &plan.operations;
        let aft_poll = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    UpgradeOperation::PollModuleRestartCompletion {
                        target: UpgradeTarget::Aft
                    }
                )
            })
            .unwrap();
        let daemon_restart = operations
            .iter()
            .position(|operation| {
                matches!(operation, UpgradeOperation::RestartDaemonViaServiceManager)
            })
            .unwrap();
        let ck_replace = operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    UpgradeOperation::ReplaceDestination {
                        target: UpgradeTarget::Ck
                    }
                )
            })
            .unwrap();
        assert!(aft_poll < daemon_restart);
        assert!(daemon_restart < ck_replace);
        assert!(!operations.iter().any(|operation| matches!(
            operation,
            UpgradeOperation::InitiateModuleRestart {
                target: UpgradeTarget::Ck
            } | UpgradeOperation::PollModuleRestartCompletion {
                target: UpgradeTarget::Ck
            }
        )));
    }
}
