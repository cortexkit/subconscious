use std::time::Duration;

use super::{
    model::{UpgradeOperation, UpgradeTarget},
    planner::UpgradePlan,
};

pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a restart is given to come back live and healthy. A restart is
/// the drain (up to `DRAIN_TIMEOUT`, spent in full whenever a consumer holds
/// a route open) followed by the module's own startup; aft answers `ok` only
/// after warming its roots, which took 75 s on a loaded host and longer on
/// a cold VM. Polling for only the drain budget refused a restart that
/// succeeded thirty seconds later and stopped the ladder before the daemon.
pub const RESTART_COMPLETION_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvidence {
    pub target: UpgradeTarget,
    pub stage: &'static str,
    pub detail: String,
}

impl std::fmt::Display for UpgradeEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "evidence: {} {}: {}",
            self.target, self.stage, self.detail
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpgradeExecutionReport {
    pub evidence: Vec<UpgradeEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RollbackDecision {
    Accepted,
    Declined,
}

pub trait UpgradeExecutionBackend {
    fn download_and_verify(&mut self, target: UpgradeTarget) -> Result<String, String>;
    fn create_rollback_copy(&mut self, target: UpgradeTarget) -> Result<String, String>;
    fn replace_destination(&mut self, target: UpgradeTarget) -> Result<String, String>;
    fn warm_execute(&mut self, target: UpgradeTarget) -> Result<String, String>;
    fn initiate_module_restart(
        &mut self,
        target: UpgradeTarget,
        drain_timeout: Duration,
    ) -> Result<String, String>;
    fn poll_module_restart_completion(
        &mut self,
        target: UpgradeTarget,
        completion_timeout: Duration,
    ) -> Result<String, String>;
    fn restart_daemon_via_service_manager(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<String, String>;
    fn poll_daemon_service_ready(&mut self, completion_timeout: Duration)
        -> Result<String, String>;
    fn post_verify(&mut self, target: UpgradeTarget) -> Result<String, String>;

    fn completed(&mut self, _target: UpgradeTarget) {}

    fn rollback_decision(&mut self, target: UpgradeTarget) -> RollbackDecision;
    fn rollback(&mut self, target: UpgradeTarget) -> Result<String, String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeExecutionFailure {
    pub target: UpgradeTarget,
    pub stage: &'static str,
    pub reason: String,
    pub report: UpgradeExecutionReport,
}

impl std::fmt::Display for UpgradeExecutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "refusal: {} {} failed: {}",
            self.target, self.stage, self.reason
        )
    }
}

impl std::error::Error for UpgradeExecutionFailure {}

/// Executes only the operations already authorized by the planner. Success
/// evidence is appended after a stage returns successfully, so a later-stage
/// failure can never make an unperformed stage look successful in the report.
pub fn execute_upgrade<B: UpgradeExecutionBackend>(
    plan: &UpgradePlan,
    backend: &mut B,
) -> Result<UpgradeExecutionReport, UpgradeExecutionFailure> {
    let mut report = UpgradeExecutionReport::default();
    for operation in &plan.operations {
        let (target, stage, result) = match operation {
            UpgradeOperation::ObservePlatform => continue,
            UpgradeOperation::DownloadAndVerify { target } => (
                *target,
                "download-and-verify",
                backend.download_and_verify(*target),
            ),
            UpgradeOperation::CreateRollbackCopy { target } => (
                *target,
                "rollback-copy",
                backend.create_rollback_copy(*target),
            ),
            UpgradeOperation::ReplaceDestination { target } => (
                *target,
                "destination-replacement",
                backend.replace_destination(*target),
            ),
            UpgradeOperation::WarmExecute { target } => {
                (*target, "warm-execution", backend.warm_execute(*target))
            }
            UpgradeOperation::InitiateModuleRestart { target } => (
                *target,
                "restart-initiation",
                backend.initiate_module_restart(*target, DRAIN_TIMEOUT),
            ),
            UpgradeOperation::PollModuleRestartCompletion { target } => (
                *target,
                "restart-completion",
                backend.poll_module_restart_completion(*target, RESTART_COMPLETION_TIMEOUT),
            ),
            UpgradeOperation::RestartDaemonViaServiceManager => (
                UpgradeTarget::Daemon,
                "service-manager-restart",
                backend.restart_daemon_via_service_manager(DRAIN_TIMEOUT),
            ),
            UpgradeOperation::PollDaemonServiceReady => (
                UpgradeTarget::Daemon,
                "service-manager-completion",
                backend.poll_daemon_service_ready(RESTART_COMPLETION_TIMEOUT),
            ),
            UpgradeOperation::PostVerify { target } => {
                let result = backend.post_verify(*target);
                if let Err(reason) = result {
                    report.evidence.push(UpgradeEvidence {
                        target: *target,
                        stage: "post-verification",
                        detail: format!("failed: {reason}"),
                    });
                    match backend.rollback_decision(*target) {
                        RollbackDecision::Accepted => match backend.rollback(*target) {
                            Ok(detail) => report.evidence.push(UpgradeEvidence {
                                target: *target,
                                stage: "rollback",
                                detail,
                            }),
                            Err(rollback_reason) => report.evidence.push(UpgradeEvidence {
                                target: *target,
                                stage: "rollback",
                                detail: format!("failed: {rollback_reason}"),
                            }),
                        },
                        RollbackDecision::Declined => report.evidence.push(UpgradeEvidence {
                            target: *target,
                            stage: "rollback-offer",
                            detail: "offered; declined; replacement remains in place (set CK_UPGRADE_ROLLBACK=accept to restore)".to_string(),
                        }),
                    }
                    return Err(UpgradeExecutionFailure {
                        target: *target,
                        stage: "post-verification",
                        reason,
                        report,
                    });
                }
                backend.completed(*target);
                (*target, "post-verification", result)
            }
        };
        match result {
            Ok(detail) => report.evidence.push(UpgradeEvidence {
                target,
                stage,
                detail,
            }),
            Err(reason) => {
                return Err(UpgradeExecutionFailure {
                    target,
                    stage,
                    reason,
                    report,
                });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::{model::UpgradeObserved, planner::plan_upgrade};

    #[derive(Default)]
    struct RecordingBackend {
        calls: Vec<&'static str>,
        fail_warm: bool,
        fail_activation: bool,
        fail_verify: bool,
        rollback: bool,
    }

    impl UpgradeExecutionBackend for RecordingBackend {
        fn download_and_verify(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("download");
            Ok("sha256 verified".to_string())
        }
        fn create_rollback_copy(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("copy");
            Ok("prior inode=1".to_string())
        }
        fn replace_destination(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("replace");
            Ok("destination replaced".to_string())
        }
        fn warm_execute(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("warm");
            if self.fail_warm {
                Err("candidate exited 1".to_string())
            } else {
                Ok("destination inode executed".to_string())
            }
        }
        fn initiate_module_restart(
            &mut self,
            _target: UpgradeTarget,
            drain_timeout: Duration,
        ) -> Result<String, String> {
            assert_eq!(drain_timeout, DRAIN_TIMEOUT);
            self.calls.push("initiate");
            if self.fail_activation {
                Err("restart acknowledgement was not received".to_string())
            } else {
                Ok("acknowledged".to_string())
            }
        }
        fn poll_module_restart_completion(
            &mut self,
            _target: UpgradeTarget,
            completion_timeout: Duration,
        ) -> Result<String, String> {
            // The completion budget must exceed the drain: a restart is the
            // drain plus the module's own startup.
            assert_eq!(completion_timeout, RESTART_COMPLETION_TIMEOUT);
            assert!(completion_timeout > DRAIN_TIMEOUT);
            self.calls.push("poll");
            Ok("healthy".to_string())
        }
        fn restart_daemon_via_service_manager(
            &mut self,
            drain_timeout: Duration,
        ) -> Result<String, String> {
            assert_eq!(drain_timeout, DRAIN_TIMEOUT);
            self.calls.push("service-restart");
            Ok("requested".to_string())
        }
        fn poll_daemon_service_ready(
            &mut self,
            completion_timeout: Duration,
        ) -> Result<String, String> {
            assert_eq!(completion_timeout, RESTART_COMPLETION_TIMEOUT);
            self.calls.push("service-poll");
            Ok("healthy".to_string())
        }
        fn post_verify(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("verify");
            if self.fail_verify {
                Err("new process has stale version".to_string())
            } else {
                Ok("pid=2 inode=2 health=healthy version=2.0.0".to_string())
            }
        }
        fn completed(&mut self, _target: UpgradeTarget) {
            self.calls.push("completed");
        }
        fn rollback_decision(&mut self, _target: UpgradeTarget) -> RollbackDecision {
            if self.rollback {
                RollbackDecision::Accepted
            } else {
                RollbackDecision::Declined
            }
        }
        fn rollback(&mut self, _target: UpgradeTarget) -> Result<String, String> {
            self.calls.push("rollback");
            Ok("restored prior inode=1".to_string())
        }
    }

    fn update_plan() -> UpgradePlan {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.supervised_modules.insert("subc-mcp".to_string());
        observed.supervised_modules.insert("aft".to_string());
        for target in UpgradeTarget::ORDERED {
            observed.targets.insert(
                target.label().to_string(),
                super::super::model::UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                    reason: None,
                },
            );
        }
        plan_upgrade(&observed)
    }

    #[test]
    fn a_failed_warm_execution_does_not_claim_restart_or_verification_success() {
        let mut backend = RecordingBackend {
            fail_warm: true,
            ..Default::default()
        };
        let failure = execute_upgrade(&update_plan(), &mut backend).expect_err("warm failure");
        // The daemon is the first target on the ladder, so its warm failure
        // stops everything before any module is touched.
        assert_eq!(failure.target, UpgradeTarget::Daemon);
        assert_eq!(backend.calls, ["download", "copy", "replace", "warm"]);
        assert!(failure
            .report
            .evidence
            .iter()
            .all(|item| item.stage != "restart-initiation"));
    }

    #[test]
    fn failed_restart_activation_does_not_claim_completion_or_verification() {
        let mut backend = RecordingBackend {
            fail_activation: true,
            ..Default::default()
        };
        let failure =
            execute_upgrade(&update_plan(), &mut backend).expect_err("activation failure");
        // The daemon rung (service restart, poll, verify) completes ahead of
        // the first module rung, whose activation is what fails here; the
        // calls that must be absent are that module's own poll and verify.
        assert_eq!(failure.stage, "restart-initiation");
        assert_eq!(failure.target, UpgradeTarget::SubcMcp);
        let daemon_verified = backend
            .calls
            .iter()
            .position(|call| *call == "verify")
            .expect("daemon rung verified before the module rung");
        let module_activation = backend
            .calls
            .iter()
            .rposition(|call| *call == "initiate")
            .expect("module activation attempted");
        assert!(daemon_verified < module_activation);
        assert!(!backend.calls[module_activation..].contains(&"poll"));
        assert!(!backend.calls[module_activation..].contains(&"verify"));
        assert!(!backend.calls[module_activation..].contains(&"completed"));
    }

    #[test]
    fn a_successful_target_is_announced_after_post_verification() {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.targets.insert(
            UpgradeTarget::Ck.label().to_string(),
            super::super::model::UpgradeState::UpdateAvailable {
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
                reason: None,
            },
        );
        let mut backend = RecordingBackend::default();

        execute_upgrade(&plan_upgrade(&observed), &mut backend).unwrap();

        assert_eq!(
            backend.calls,
            ["download", "copy", "replace", "warm", "verify", "completed"]
        );
    }

    #[test]
    fn accepted_rollback_reports_the_restored_prior_inode() {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.targets.insert(
            UpgradeTarget::Ck.label().to_string(),
            super::super::model::UpgradeState::UpdateAvailable {
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
                reason: None,
            },
        );
        let mut backend = RecordingBackend {
            fail_verify: true,
            rollback: true,
            ..Default::default()
        };
        let failure =
            execute_upgrade(&plan_upgrade(&observed), &mut backend).expect_err("verify failure");
        assert!(failure
            .report
            .evidence
            .iter()
            .any(|item| item.stage == "rollback" && item.detail.contains("prior inode=1")));
    }

    #[test]
    fn roster_lacking_subc_mcp_and_having_aft_plans_no_subc_mcp_restart_and_executes_to_completion()
    {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.supervised_modules.insert("aft".to_string());
        for target in [UpgradeTarget::SubcMcp, UpgradeTarget::Aft] {
            observed.targets.insert(
                target.label().to_string(),
                super::super::model::UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                    reason: None,
                },
            );
        }
        let plan = plan_upgrade(&observed);

        assert!(!plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::InitiateModuleRestart {
                target: UpgradeTarget::SubcMcp
            } | UpgradeOperation::PollModuleRestartCompletion {
                target: UpgradeTarget::SubcMcp
            }
        )));
        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::InitiateModuleRestart {
                target: UpgradeTarget::Aft
            }
        )));
        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::PollModuleRestartCompletion {
                target: UpgradeTarget::Aft
            }
        )));

        let mut backend = RecordingBackend::default();
        let report = execute_upgrade(&plan, &mut backend).expect("executes to completion");
        assert_eq!(
            backend.calls,
            [
                "download",
                "copy",
                "replace",
                "warm",
                "verify",
                "completed",
                "download",
                "copy",
                "replace",
                "warm",
                "initiate",
                "poll",
                "verify",
                "completed"
            ]
        );
        assert!(report
            .evidence
            .iter()
            .any(|e| e.target == UpgradeTarget::SubcMcp && e.stage == "post-verification"));
        assert!(report
            .evidence
            .iter()
            .any(|e| e.target == UpgradeTarget::Aft && e.stage == "restart-completion"));
    }

    #[test]
    fn roster_having_both_subc_mcp_and_aft_plans_both_restart_pairs() {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.supervised_modules.insert("subc-mcp".to_string());
        observed.supervised_modules.insert("aft".to_string());
        for target in [UpgradeTarget::SubcMcp, UpgradeTarget::Aft] {
            observed.targets.insert(
                target.label().to_string(),
                super::super::model::UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                    reason: None,
                },
            );
        }
        let plan = plan_upgrade(&observed);

        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::InitiateModuleRestart {
                target: UpgradeTarget::SubcMcp
            }
        )));
        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::PollModuleRestartCompletion {
                target: UpgradeTarget::SubcMcp
            }
        )));
        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::InitiateModuleRestart {
                target: UpgradeTarget::Aft
            }
        )));
        assert!(plan.operations.iter().any(|op| matches!(
            op,
            UpgradeOperation::PollModuleRestartCompletion {
                target: UpgradeTarget::Aft
            }
        )));
    }
}
