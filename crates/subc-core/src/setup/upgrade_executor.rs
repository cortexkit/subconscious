use std::time::Duration;

use super::{
    model::{UpgradeOperation, UpgradeTarget},
    planner::UpgradePlan,
};

pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

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
        drain_timeout: Duration,
    ) -> Result<String, String>;
    fn restart_daemon_via_service_manager(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<String, String>;
    fn poll_daemon_service_ready(&mut self, drain_timeout: Duration) -> Result<String, String>;
    fn post_verify(&mut self, target: UpgradeTarget) -> Result<String, String>;
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
                backend.poll_module_restart_completion(*target, DRAIN_TIMEOUT),
            ),
            UpgradeOperation::RestartDaemonViaServiceManager => (
                UpgradeTarget::Daemon,
                "service-manager-restart",
                backend.restart_daemon_via_service_manager(DRAIN_TIMEOUT),
            ),
            UpgradeOperation::PollDaemonServiceReady => (
                UpgradeTarget::Daemon,
                "service-manager-completion",
                backend.poll_daemon_service_ready(DRAIN_TIMEOUT),
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
            drain_timeout: Duration,
        ) -> Result<String, String> {
            assert_eq!(drain_timeout, DRAIN_TIMEOUT);
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
        fn poll_daemon_service_ready(&mut self, drain_timeout: Duration) -> Result<String, String> {
            assert_eq!(drain_timeout, DRAIN_TIMEOUT);
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
        for target in UpgradeTarget::ORDERED {
            observed.targets.insert(
                target.label().to_string(),
                super::super::model::UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
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
        assert_eq!(failure.target, UpgradeTarget::SubcMcp);
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
        assert_eq!(failure.stage, "restart-initiation");
        assert!(!backend.calls.contains(&"poll"));
        assert!(!backend.calls.contains(&"verify"));
    }

    #[test]
    fn accepted_rollback_reports_the_restored_prior_inode() {
        let mut observed = UpgradeObserved::no_updates_on_current_host();
        observed.targets.insert(
            UpgradeTarget::Ck.label().to_string(),
            super::super::model::UpgradeState::UpdateAvailable {
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
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
}
