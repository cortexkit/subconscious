//! The terminal app's update wizard, run by the setup engine.
//!
//! [`SetupUpgradeService`] implements ck-ui's upgrade service with the same
//! steps `ck upgrade` takes: it discovers what `ck setup` installed, refuses
//! with the same words when ownership is incomplete or the plan is refused,
//! plans with the signed release index, and runs the plan through the
//! engine. A wrapping backend reports each stage's start and result to the
//! wizard as it happens.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use ck_ui::upgrade::{
    PlanRow, RollbackChoice, RollbackQuestion, StageEvent, StageRef, StageStatus, UpgradeCheck,
    UpgradeEvent, UpgradeOutcome, UpgradeService,
};

use super::model::{UpgradeObserved, UpgradeOperation, UpgradeState, UpgradeTarget};
use super::planner::UpgradePlan;
use super::upgrade::{ManagedUpgradeTarget, SystemUpgradeBackend, UpgradeOutput};
use super::upgrade_executor::{RollbackDecision, UpgradeExecutionBackend};
use super::PlanOutcome;

/// What the wizard needs from the daemon `ck` talks to, answered the way
/// `ck upgrade` answers it.
pub trait UpgradeDaemon: Send + Sync + 'static {
    /// The connection file post-activation checks read.
    fn connection_path(&self) -> Option<PathBuf>;
    /// The ids of the modules the daemon supervises, or why it couldn't be
    /// asked. A module the daemon supervises is restarted onto its new
    /// binary; the plan refuses a module upgrade while the daemon is
    /// unreachable.
    fn supervised_modules(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<String>, String>> + Send + '_>>;
}

/// The setup engine behind Settings → Updates.
pub struct SetupUpgradeService {
    executable: PathBuf,
    daemon: Box<dyn UpgradeDaemon>,
    /// The runtime the release index check and the roster request run on.
    /// The wizard calls this service from a blocking thread.
    runtime: tokio::runtime::Handle,
}

/// A plan ready to run: everything the engine's backend is set up from.
struct Ready {
    plan: UpgradePlan,
    observed: UpgradeObserved,
    discovered: Vec<ManagedUpgradeTarget>,
    index: super::release_index::ReleaseIndex,
}

impl SetupUpgradeService {
    /// `executable` is the running `ck`, which the plan replaces last.
    pub fn new(
        executable: impl Into<PathBuf>,
        daemon: Box<dyn UpgradeDaemon>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            executable: executable.into(),
            daemon,
            runtime,
        }
    }

    /// Plans as `ck upgrade` does before it changes anything. Returns what
    /// the wizard shows, and the plan to run when there is one. Nothing is
    /// written: the engine's backend, which tightens permissions and sweeps
    /// old images when it starts, is set up only by [`Self::run`].
    async fn plan(&self) -> Result<(UpgradeCheck, Option<Ready>), String> {
        let discovered = super::discover_current_upgrade_targets(
            &self.executable,
            None::<&super::DaemonCatalogBuild>,
        )?;
        let ownership = super::upgrade_ownership_outcomes(&self.executable, &discovered)?;
        if let Some(refusal) = super::text::upgrade_ownership_refusal(&ownership) {
            return Ok((refused(refusal), None));
        }
        let cache = super::UpdateCache::from_environment();
        let source = super::IndexReleaseSource::from_environment(super::TARGET_CHECK_BUDGET);
        let roster: Vec<UpgradeTarget> = discovered.iter().map(|item| item.target).collect();
        let metadata = match super::check_update_metadata(&cache, &source, &roster).await {
            Ok(metadata) => metadata,
            Err(super::UpdateCheckError::IndexUnreachable { .. }) => {
                return Err(super::not_checked_from_cache(&cache).render())
            }
            Err(error) => return Err(error.to_string()),
        };
        let modules = self.daemon.supervised_modules().await;
        let index = source.cloned_index().map_err(|error| error.to_string())?;
        let observed =
            super::observed_upgrade_targets(&metadata, &discovered, modules, Some(&index));
        let plan = super::plan_upgrade(&observed);
        let check = UpgradeCheck {
            rows: plan_rows(&discovered, &observed, &plan),
            refusal: super::text::upgrade_plan_refusal(&plan),
            stages: plan_stages(&plan),
        };
        let ready = (check.refusal.is_none()
            && plan.operations.iter().any(UpgradeOperation::mutates))
        .then_some(Ready {
            plan,
            observed,
            discovered,
            index,
        });
        Ok((check, ready))
    }
}

fn refused(refusal: String) -> UpgradeCheck {
    UpgradeCheck {
        rows: Vec::new(),
        refusal: Some(refusal),
        stages: Vec::new(),
    }
}

impl UpgradeService for SetupUpgradeService {
    fn check(&self) -> Result<UpgradeCheck, String> {
        self.runtime.block_on(self.plan()).map(|(check, _)| check)
    }

    fn run(
        &self,
        report: &mut dyn FnMut(UpgradeEvent),
        ask: &mut dyn FnMut(RollbackQuestion) -> Option<RollbackChoice>,
    ) -> UpgradeOutcome {
        let stopped = |error: String| UpgradeOutcome {
            error: Some(error),
            ..UpgradeOutcome::default()
        };
        let (check, ready) = match self.runtime.block_on(self.plan()) {
            Ok(planned) => planned,
            Err(error) => return stopped(error),
        };
        report(UpgradeEvent::Planned(check.clone()));
        if let Some(refusal) = check.refusal {
            return stopped(refusal);
        }
        let Some(ready) = ready else {
            return UpgradeOutcome::default();
        };
        let mut backend = match SystemUpgradeBackend::new(
            &self.executable,
            self.daemon.connection_path(),
            ready.discovered,
            ready.index,
            // The app owns the terminal, so the engine keeps its lines for it
            // from the moment it is built.
            UpgradeOutput::Kept,
        ) {
            Ok(backend) => backend,
            Err(error) => return stopped(error),
        };
        backend.set_supervised_modules(ready.observed.supervised_modules.clone());
        let mut versions = BTreeMap::new();
        let mut previous = BTreeMap::new();
        for &target in &ready.observed.roster {
            if let UpgradeState::UpdateAvailable { from, to, .. } =
                ready.observed.target_state(target)
            {
                previous.insert(target.label(), from.clone());
                backend.set_planned_from(target, from);
                backend.set_expected_version(target, to.clone());
                versions.insert(target.label(), to);
            }
        }
        let mut reporting = Reporting {
            inner: backend,
            report,
            ask,
            daemon: daemon_target(&ready.plan),
            versions,
            previous,
            failure: None,
            installed: Vec::new(),
        };
        let result = super::execute_upgrade(&ready.plan, &mut reporting);
        UpgradeOutcome {
            installed: reporting.installed,
            error: result.err().map(|failure| failure.to_string()),
            warnings: reporting.inner.take_warnings(),
        }
    }
}

/// The daemon target, which the service-manager stages restart without
/// naming it.
fn daemon_target(plan: &UpgradePlan) -> Option<UpgradeTarget> {
    plan.operations
        .iter()
        .find_map(|operation| match operation {
            UpgradeOperation::RestartDaemonViaServiceManager { target } => Some(*target),
            _ => None,
        })
}

/// The stage id the engine records for an operation, as its evidence and
/// failures name it; `None` for the platform check, which is no stage.
pub fn operation_stage(operation: &UpgradeOperation) -> Option<(UpgradeTarget, &'static str)> {
    Some(match operation {
        UpgradeOperation::ObservePlatform => return None,
        UpgradeOperation::DownloadAndVerify { target } => (*target, "download-and-verify"),
        UpgradeOperation::CreateRollbackCopy { target } => (*target, "rollback-copy"),
        UpgradeOperation::ReplaceDestination { target } => (*target, "destination-replacement"),
        UpgradeOperation::WarmExecute { target } => (*target, "warm-execution"),
        UpgradeOperation::InitiateModuleRestart { target } => (*target, "restart-initiation"),
        UpgradeOperation::PollModuleRestartCompletion { target } => (*target, "restart-completion"),
        UpgradeOperation::RestartDaemonViaServiceManager { target } => {
            (*target, "service-manager-restart")
        }
        UpgradeOperation::PollDaemonServiceReady { target } => {
            (*target, "service-manager-completion")
        }
        UpgradeOperation::PostVerify { target } => (*target, "post-verification"),
    })
}

/// Every stage the plan's operations take, in order.
pub fn plan_stages(plan: &UpgradePlan) -> Vec<StageRef> {
    plan.operations
        .iter()
        .filter_map(operation_stage)
        .map(|(target, stage)| StageRef {
            target: target.label().to_string(),
            stage: stage.to_string(),
        })
        .collect()
}

/// A row per discovered component, ck first, saying what the plan does to
/// it.
pub fn plan_rows(
    discovered: &[ManagedUpgradeTarget],
    observed: &UpgradeObserved,
    plan: &UpgradePlan,
) -> Vec<PlanRow> {
    let ordered = discovered
        .iter()
        .filter(|item| item.target.is_self_replacing())
        .chain(
            discovered
                .iter()
                .filter(|item| !item.target.is_self_replacing()),
        );
    ordered
        .map(|item| {
            let target = item.target;
            let replaced = plan
                .operations
                .contains(&UpgradeOperation::ReplaceDestination { target });
            let restarted = plan
                .operations
                .contains(&UpgradeOperation::InitiateModuleRestart { target });
            let (installed, new, what) = match observed.target_state(target) {
                UpgradeState::UpdateAvailable { from, to, .. } => {
                    let what = if !replaced {
                        refusal_for(plan, target).unwrap_or_else(|| "not upgraded".to_string())
                    } else if target.is_daemon() {
                        "replace · restarts everything".to_string()
                    } else if let (true, Some(module)) = (restarted, target.module_id()) {
                        format!("replace · restarts {module}")
                    } else {
                        "replace".to_string()
                    };
                    (from, to, what)
                }
                _ => (
                    item.installed_version.clone(),
                    item.installed_version.clone(),
                    "up to date".to_string(),
                ),
            };
            PlanRow {
                component: if target.is_daemon() {
                    format!("daemon ({target})")
                } else {
                    target.to_string()
                },
                installed,
                new,
                what,
                replaces: replaced,
                daemon: target.is_daemon(),
            }
        })
        .collect()
}

/// Why the plan leaves `target` out, when it says so for that target alone.
fn refusal_for(plan: &UpgradePlan, target: UpgradeTarget) -> Option<String> {
    plan.outcomes.iter().find_map(|outcome| match outcome {
        PlanOutcome::TargetRefused { component, reason } if *component == target.component => {
            Some(format!("not upgraded: {reason}"))
        }
        _ => None,
    })
}

/// The engine's backend, reporting each stage's start and result as the
/// engine calls it. The engine calls a stage's method when the stage starts
/// and records the method's `Ok` or `Err` as its result, so a report from
/// here is exactly what the engine's evidence records.
///
/// When the engine offers a rollback, the person decides through `ask`.
/// The inner backend's own decision is never consulted: it reads
/// `CK_UPGRADE_ROLLBACK`, which is how `ck upgrade` decides without a
/// person, and the wizard has one.
struct Reporting<'a, B> {
    inner: B,
    report: &'a mut dyn FnMut(UpgradeEvent),
    ask: &'a mut dyn FnMut(RollbackQuestion) -> Option<RollbackChoice>,
    daemon: Option<UpgradeTarget>,
    /// Each replaced target's new version, for the installed list and the
    /// rollback question.
    versions: BTreeMap<&'static str, String>,
    /// Each replaced target's version before the run, which a rollback
    /// restores.
    previous: BTreeMap<&'static str, String>,
    /// The reason the last failed stage gave, which the rollback question
    /// that follows quotes.
    failure: Option<String>,
    /// "<label> <version>" for each target the engine verified.
    installed: Vec<String>,
}

impl<B> Reporting<'_, B> {
    fn stage(
        &mut self,
        target: Option<UpgradeTarget>,
        stage: &'static str,
        call: impl FnOnce(&mut B) -> Result<String, String>,
    ) -> Result<String, String> {
        // Only the service-manager stages pass no target. The plan names
        // the daemon target with them, so the fallback never shows in
        // practice; it reads as the daemon's binary if it ever did.
        let label = target.map_or("ck-subc", UpgradeTarget::label);
        let event = |status| {
            UpgradeEvent::Stage(StageEvent {
                target: label.to_string(),
                stage: stage.to_string(),
                status,
            })
        };
        (self.report)(event(StageStatus::Started));
        let result = call(&mut self.inner);
        if let Err(reason) = &result {
            self.failure = Some(reason.clone());
        }
        (self.report)(event(match &result {
            Ok(detail) => StageStatus::Ok(detail.clone()),
            Err(reason) => StageStatus::Failed(reason.clone()),
        }));
        result
    }
}

impl<B: UpgradeExecutionBackend> UpgradeExecutionBackend for Reporting<'_, B> {
    fn download_and_verify(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "download-and-verify", |b| {
            b.download_and_verify(target)
        })
    }

    fn create_rollback_copy(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "rollback-copy", |b| {
            b.create_rollback_copy(target)
        })
    }

    fn replace_destination(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "destination-replacement", |b| {
            b.replace_destination(target)
        })
    }

    fn warm_execute(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "warm-execution", |b| b.warm_execute(target))
    }

    fn initiate_module_restart(
        &mut self,
        target: UpgradeTarget,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        self.stage(Some(target), "restart-initiation", |b| {
            b.initiate_module_restart(target, drain_timeout)
        })
    }

    fn poll_module_restart_completion(
        &mut self,
        target: UpgradeTarget,
        completion_timeout: Duration,
    ) -> Result<String, String> {
        self.stage(Some(target), "restart-completion", |b| {
            b.poll_module_restart_completion(target, completion_timeout)
        })
    }

    fn restart_daemon_via_service_manager(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        let daemon = self.daemon;
        self.stage(daemon, "service-manager-restart", |b| {
            b.restart_daemon_via_service_manager(drain_timeout)
        })
    }

    fn poll_daemon_service_ready(
        &mut self,
        completion_timeout: Duration,
    ) -> Result<String, String> {
        let daemon = self.daemon;
        self.stage(daemon, "service-manager-completion", |b| {
            b.poll_daemon_service_ready(completion_timeout)
        })
    }

    fn post_verify(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "post-verification", |b| b.post_verify(target))
    }

    fn completed(&mut self, target: UpgradeTarget) {
        self.inner.completed(target);
        let version = self
            .versions
            .get(target.label())
            .cloned()
            .unwrap_or_default();
        self.installed.push(
            format!("{} {version}", target.label())
                .trim_end()
                .to_string(),
        );
    }

    fn rollback_decision(&mut self, target: UpgradeTarget) -> RollbackDecision {
        let label = target.label();
        let question = RollbackQuestion {
            component: label.to_string(),
            old: self.previous.get(label).cloned().unwrap_or_default(),
            new: self.versions.get(label).cloned().unwrap_or_default(),
            reason: self.failure.take().unwrap_or_default(),
        };
        match (self.ask)(question) {
            Some(RollbackChoice::RollBack) => RollbackDecision::Accepted,
            // Without an answer the new version stays, as `ck upgrade`
            // keeps it unless told to roll back.
            Some(RollbackChoice::Keep) | None => RollbackDecision::Declined,
        }
    }

    fn rollback(&mut self, target: UpgradeTarget) -> Result<String, String> {
        self.stage(Some(target), "rollback", |b| b.rollback(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::components::upgrade_roster;
    use crate::setup::model::Component;
    use crate::setup::planner::plan_upgrade;

    /// Answers every stage `Ok("<stage> done")`, except `fail`, which fails
    /// with "<stage> broke". Its own rollback decision must never be asked:
    /// in `ck upgrade` that decision reads the environment, and the wizard
    /// asks the person instead.
    struct Scripted {
        fail: Option<&'static str>,
        completed: Vec<&'static str>,
    }

    impl Scripted {
        fn answer(&self, stage: &'static str) -> Result<String, String> {
            if self.fail == Some(stage) {
                Err(format!("{stage} broke"))
            } else {
                Ok(format!("{stage} done"))
            }
        }
    }

    impl UpgradeExecutionBackend for Scripted {
        fn download_and_verify(&mut self, _: UpgradeTarget) -> Result<String, String> {
            self.answer("download-and-verify")
        }
        fn create_rollback_copy(&mut self, _: UpgradeTarget) -> Result<String, String> {
            self.answer("rollback-copy")
        }
        fn replace_destination(&mut self, _: UpgradeTarget) -> Result<String, String> {
            self.answer("destination-replacement")
        }
        fn warm_execute(&mut self, _: UpgradeTarget) -> Result<String, String> {
            self.answer("warm-execution")
        }
        fn initiate_module_restart(
            &mut self,
            _: UpgradeTarget,
            _: Duration,
        ) -> Result<String, String> {
            self.answer("restart-initiation")
        }
        fn poll_module_restart_completion(
            &mut self,
            _: UpgradeTarget,
            _: Duration,
        ) -> Result<String, String> {
            self.answer("restart-completion")
        }
        fn restart_daemon_via_service_manager(&mut self, _: Duration) -> Result<String, String> {
            self.answer("service-manager-restart")
        }
        fn poll_daemon_service_ready(&mut self, _: Duration) -> Result<String, String> {
            self.answer("service-manager-completion")
        }
        fn post_verify(&mut self, _: UpgradeTarget) -> Result<String, String> {
            self.answer("post-verification")
        }
        fn completed(&mut self, target: UpgradeTarget) {
            self.completed.push(target.label());
        }
        fn rollback_decision(&mut self, _: UpgradeTarget) -> RollbackDecision {
            panic!("the wizard asked the backend, not the person, whether to roll back")
        }
        fn rollback(&mut self, _: UpgradeTarget) -> Result<String, String> {
            Ok("restored the previous binary".to_string())
        }
    }

    /// The daemon, the MCP gateway and aft, each one release behind, with
    /// both modules supervised.
    fn observed() -> UpgradeObserved {
        let mut observed =
            UpgradeObserved::for_roster(upgrade_roster([Component::Core, Component::Aft]));
        observed.supervised_modules.insert("subc-mcp".to_string());
        observed.supervised_modules.insert("aft".to_string());
        for target in observed.roster.clone() {
            observed.targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                    reason: None,
                },
            );
        }
        observed
    }

    /// What a scripted run reported: its stage events, what it installed,
    /// and every rollback question it asked.
    struct Ran {
        events: Vec<StageEvent>,
        installed: Vec<String>,
        asked: Vec<RollbackQuestion>,
    }

    /// Runs `plan` against the scripted backend, failing the `fail` stage,
    /// and answers any rollback question with `answer`.
    fn run_answering(
        plan: &UpgradePlan,
        fail: Option<&'static str>,
        answer: Option<RollbackChoice>,
    ) -> Ran {
        let mut events = Vec::new();
        let mut report = |event| {
            if let UpgradeEvent::Stage(event) = event {
                events.push(event);
            }
        };
        let mut asked = Vec::new();
        let mut ask = |question| {
            asked.push(question);
            answer
        };
        let mut reporting = Reporting {
            inner: Scripted {
                fail,
                completed: Vec::new(),
            },
            report: &mut report,
            ask: &mut ask,
            daemon: daemon_target(plan),
            versions: BTreeMap::from([("ck-subc", "2.0.0".to_string())]),
            previous: BTreeMap::from([("ck-subc", "1.0.0".to_string())]),
            failure: None,
            installed: Vec::new(),
        };
        let _ = super::super::execute_upgrade(plan, &mut reporting);
        let installed = reporting.installed;
        Ran {
            events,
            installed,
            asked,
        }
    }

    fn run(plan: &UpgradePlan, fail: Option<&'static str>) -> (Vec<StageEvent>, Vec<String>) {
        let ran = run_answering(plan, fail, Some(RollbackChoice::RollBack));
        (ran.events, ran.installed)
    }

    #[test]
    fn every_stage_the_plan_lists_reports_its_start_and_result_in_order() {
        let plan = plan_upgrade(&observed());
        let planned = plan_stages(&plan);
        assert!(planned.iter().any(|s| s.stage == "service-manager-restart"));
        assert!(planned.iter().any(|s| s.stage == "restart-initiation"));
        let (events, installed) = run(&plan, None);
        let mut expected = Vec::new();
        for s in &planned {
            for status in [
                StageStatus::Started,
                StageStatus::Ok(format!("{} done", s.stage)),
            ] {
                expected.push(StageEvent {
                    target: s.target.clone(),
                    stage: s.stage.clone(),
                    status,
                });
            }
        }
        assert_eq!(events, expected);
        assert_eq!(installed[0], "ck-subc 2.0.0");
        assert_eq!(installed.len(), observed().roster.len());
    }

    #[test]
    fn a_failed_stage_reports_its_reason_as_given_and_the_rollback_that_follows() {
        let plan = plan_upgrade(&observed());
        let (events, installed) = run(&plan, Some("warm-execution"));
        let failed = events
            .iter()
            .position(|e| e.status == StageStatus::Failed("warm-execution broke".into()))
            .expect("the failure is reported");
        assert_eq!(events[failed].target, "ck-subc");
        assert_eq!(events[failed].stage, "warm-execution");
        assert_eq!(
            events[failed + 1..],
            [
                StageEvent {
                    target: "ck-subc".into(),
                    stage: "rollback".into(),
                    status: StageStatus::Started,
                },
                StageEvent {
                    target: "ck-subc".into(),
                    stage: "rollback".into(),
                    status: StageStatus::Ok("restored the previous binary".into()),
                },
            ]
        );
        assert!(installed.is_empty());
    }

    /// The daemon's check after its restart fails. The person is asked,
    /// with the reason exactly as the stage gave it, and the engine does
    /// what they answer: ⏎ rolls back, esc keeps the new version, and no
    /// answer at all keeps it too.
    #[test]
    fn a_failed_check_asks_the_person_and_the_engine_does_what_they_answer() {
        let plan = plan_upgrade(&observed());
        let question = RollbackQuestion {
            component: "ck-subc".into(),
            old: "1.0.0".into(),
            new: "2.0.0".into(),
            reason: "post-verification broke".into(),
        };
        let rollback = |events: &[StageEvent]| {
            events
                .iter()
                .filter(|e| e.stage == "rollback")
                .map(|e| e.status.clone())
                .collect::<Vec<_>>()
        };

        let ran = run_answering(
            &plan,
            Some("post-verification"),
            Some(RollbackChoice::RollBack),
        );
        assert_eq!(ran.asked, std::slice::from_ref(&question));
        assert_eq!(
            question.sentence(),
            "ck-subc 2.0.0 failed its check: post-verification broke."
        );
        assert_eq!(
            rollback(&ran.events),
            [
                StageStatus::Started,
                StageStatus::Ok("restored the previous binary".into())
            ]
        );
        assert!(ran.installed.is_empty());

        for answer in [Some(RollbackChoice::Keep), None] {
            let ran = run_answering(&plan, Some("post-verification"), answer);
            assert_eq!(ran.asked, std::slice::from_ref(&question), "{answer:?}");
            assert!(
                rollback(&ran.events).is_empty(),
                "{answer:?} rolled back: {:?}",
                ran.events
            );
        }
    }

    #[test]
    fn rows_say_what_each_replacement_restarts_and_which_is_the_daemon() {
        let observed = observed();
        let plan = plan_upgrade(&observed);
        let discovered: Vec<ManagedUpgradeTarget> = observed
            .roster
            .iter()
            .map(|&target| ManagedUpgradeTarget {
                target,
                destination: PathBuf::from(target.label()),
                installed_version: "1.0.0".to_string(),
                installed_archive_sha256: None,
            })
            .collect();
        let rows = plan_rows(&discovered, &observed, &plan);
        let daemon = rows.iter().find(|r| r.daemon).expect("a daemon row");
        assert_eq!(daemon.component, "daemon (ck-subc)");
        assert_eq!(daemon.what, "replace · restarts everything");
        assert!(daemon.replaces_daemon());
        let aft = rows.iter().find(|r| r.component == "ck-aft").unwrap();
        assert_eq!(aft.what, "replace · restarts aft");
        assert_eq!(
            (aft.installed.as_str(), aft.new.as_str()),
            ("1.0.0", "2.0.0")
        );
        assert_eq!(rows.iter().filter(|r| r.daemon).count(), 1);
    }

    #[test]
    fn a_refused_plan_shows_the_words_ck_upgrade_prints() {
        let mut observed = observed();
        observed.daemon_unreachable_reason = Some("connection refused".to_string());
        observed.supervised_modules.clear();
        let plan = plan_upgrade(&observed);
        let refusal = super::super::text::upgrade_plan_refusal(&plan).expect("refused");
        assert_eq!(
            refusal,
            plan.outcomes
                .iter()
                .find(|o| o.blocks_execution())
                .map(super::super::text::human_blocking_outcome)
                .unwrap()
        );
        assert!(refusal.contains("the daemon is unreachable: connection refused"));
    }
}
