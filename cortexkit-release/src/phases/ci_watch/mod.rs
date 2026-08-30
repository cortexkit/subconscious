//! Parameterized GitHub Actions `ci_watch` phase execution.
//!
//! A watch captures one run at entry, records that capture before polling, and
//! never reselects a later workflow run. Each instance owns its settle policy
//! and rerun counter so separate watches cannot borrow one another's state.

use crate::{
    provider::{
        GitHubProvider, GitHubRepository, MatchingEvidence, ProviderError, SettleParameters,
        UndecidableObservation, WorkflowJob, WorkflowJobState, WorkflowRun, WorkflowRunCapture,
        WorkflowRunPoll, WorkflowRunRequest,
    },
    PhaseInstanceId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Immutable declaration-derived parameters for one ci-watch instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CiWatchConfig {
    pub instance: PhaseInstanceId,
    pub repository: GitHubRepository,
    pub workflow: String,
    pub selector: String,
    pub settle: SettleParameters,
    pub rerun_budget: u32,
}

impl CiWatchConfig {
    /// Creates one independently configured ci-watch instance.
    pub fn new(
        instance: PhaseInstanceId,
        repository: GitHubRepository,
        workflow: impl Into<String>,
        selector: impl Into<String>,
        settle: SettleParameters,
        rerun_budget: u32,
    ) -> Result<Self, CiWatchError> {
        let workflow = workflow.into();
        let selector = selector.into();
        if instance.as_str().trim().is_empty()
            || workflow.trim().is_empty()
            || selector.trim().is_empty()
        {
            return Err(CiWatchError::InvalidConfig(
                "ci_watch requires non-empty instance, workflow, and selector".to_owned(),
            ));
        }
        Ok(Self {
            instance,
            repository,
            workflow,
            selector,
            settle,
            rerun_budget,
        })
    }

    fn run_request(&self) -> Result<WorkflowRunRequest, CiWatchError> {
        WorkflowRunRequest::new(
            self.repository.clone(),
            self.workflow.clone(),
            self.selector.clone(),
            self.settle,
        )
        .map_err(CiWatchError::Provider)
    }
}

/// Durable event emitted by one ci-watch instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CiWatchJournalRecord {
    /// The selected run is persisted before the phase polls it.
    RunCaptured {
        instance: PhaseInstanceId,
        workflow: String,
        selector: String,
        run: WorkflowRun,
        rerun_budget: u32,
    },
    /// A non-final observation that must be retried later.
    Undecidable {
        instance: PhaseInstanceId,
        run_id: Option<String>,
        observation: UndecidableObservation,
    },
    /// `gh run rerun --failed` was admitted against this captured run.
    RerunFailed {
        instance: PhaseInstanceId,
        run_id: String,
        rerun_number: u32,
    },
    /// The GitHub job rerun endpoint was admitted for a cancelled job.
    RerunByJobId {
        instance: PhaseInstanceId,
        run_id: String,
        job_id: String,
        rerun_number: u32,
    },
    /// A watched run completed successfully.
    Succeeded {
        instance: PhaseInstanceId,
        run_id: String,
        evidence: MatchingEvidence,
    },
    /// A watched run failed after its instance-owned budget was exhausted.
    Failed {
        instance: PhaseInstanceId,
        run_id: String,
        evidence: MatchingEvidence,
        first_failed_job: Option<WorkflowJob>,
        reruns_used: u32,
        rerun_budget: u32,
    },
}

/// Journal sink used by ci-watch without coupling phase logic to storage encoding.
pub trait CiWatchJournal {
    fn append_ci_watch(&mut self, record: CiWatchJournalRecord) -> Result<(), CiWatchError>;
}

/// The observable state of a ci-watch after one blocking poll attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CiWatchConclusion {
    Succeeded(MatchingEvidence),
    Failed {
        evidence: MatchingEvidence,
        first_failed_job: Option<WorkflowJob>,
    },
    Undecidable(UndecidableObservation),
}

/// Failures that are not workflow success, failure, or an instructed retry.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CiWatchError {
    #[error("invalid ci_watch configuration: {0}")]
    InvalidConfig(String),
    #[error("ci_watch provider error: {0}")]
    Provider(ProviderError),
    #[error("ci_watch journal error: {0}")]
    Journal(String),
}

/// Stateful executor for one independently declared ci-watch instance.
#[derive(Clone, Debug)]
pub struct CiWatch {
    config: CiWatchConfig,
    run: Option<WorkflowRun>,
    reruns_used: u32,
}

impl CiWatch {
    pub fn new(config: CiWatchConfig) -> Self {
        Self {
            config,
            run: None,
            reruns_used: 0,
        }
    }

    pub fn config(&self) -> &CiWatchConfig {
        &self.config
    }

    /// Returns the one captured run, if run selection has become decidable.
    pub fn run(&self) -> Option<&WorkflowRun> {
        self.run.as_ref()
    }

    pub fn reruns_used(&self) -> u32 {
        self.reruns_used
    }

    /// Captures once, journals that identity, then polls only the captured run.
    pub fn step<P: GitHubProvider, J: CiWatchJournal>(
        &mut self,
        provider: &mut P,
        journal: &mut J,
    ) -> Result<CiWatchConclusion, CiWatchError> {
        if self.run.is_none() {
            match provider.capture_workflow_run(&self.config.run_request()?) {
                Ok(WorkflowRunCapture::Captured(run)) => {
                    journal.append_ci_watch(CiWatchJournalRecord::RunCaptured {
                        instance: self.config.instance.clone(),
                        workflow: self.config.workflow.clone(),
                        selector: self.config.selector.clone(),
                        run: run.clone(),
                        rerun_budget: self.config.rerun_budget,
                    })?;
                    self.run = Some(run);
                }
                Ok(WorkflowRunCapture::Undecidable(observation)) => {
                    return self.record_undecidable(journal, None, observation);
                }
                Err(error) if error.is_transient() => {
                    return self.record_undecidable(
                        journal,
                        None,
                        self.transport_retry("capturing the GitHub workflow run", &error),
                    );
                }
                Err(error) => return Err(CiWatchError::Provider(error)),
            }
        }

        let run = self
            .run
            .as_ref()
            .expect("captured run must exist before ci_watch polling")
            .clone();
        match provider.poll_workflow_run(&run) {
            Ok(WorkflowRunPoll::Succeeded(evidence)) => {
                journal.append_ci_watch(CiWatchJournalRecord::Succeeded {
                    instance: self.config.instance.clone(),
                    run_id: run.id.to_string(),
                    evidence: evidence.clone(),
                })?;
                Ok(CiWatchConclusion::Succeeded(evidence))
            }
            Ok(WorkflowRunPoll::Undecidable(observation)) => {
                self.record_undecidable(journal, Some(run.id.to_string()), observation)
            }
            Ok(WorkflowRunPoll::Failed(evidence)) => {
                self.handle_decided_failure(provider, journal, &run, evidence)
            }
            Err(error) if error.is_transient() => self.record_undecidable(
                journal,
                Some(run.id.to_string()),
                self.transport_retry("polling the captured GitHub workflow run", &error),
            ),
            Err(error) => Err(CiWatchError::Provider(error)),
        }
    }

    fn handle_decided_failure<P: GitHubProvider, J: CiWatchJournal>(
        &mut self,
        provider: &mut P,
        journal: &mut J,
        run: &WorkflowRun,
        evidence: MatchingEvidence,
    ) -> Result<CiWatchConclusion, CiWatchError> {
        let first_failed_job = match provider.first_failed_job(run) {
            Ok(job) => job,
            Err(error) if error.is_transient() => {
                return self.record_undecidable(
                    journal,
                    Some(run.id.to_string()),
                    self.transport_retry(
                        "fetching diagnostics for the failed GitHub workflow run",
                        &error,
                    ),
                );
            }
            Err(error) => return Err(CiWatchError::Provider(error)),
        };

        if self.reruns_used >= self.config.rerun_budget {
            journal.append_ci_watch(CiWatchJournalRecord::Failed {
                instance: self.config.instance.clone(),
                run_id: run.id.to_string(),
                evidence: evidence.clone(),
                first_failed_job: first_failed_job.clone(),
                reruns_used: self.reruns_used,
                rerun_budget: self.config.rerun_budget,
            })?;
            return Ok(CiWatchConclusion::Failed {
                evidence,
                first_failed_job,
            });
        }

        let rerun_number = self.reruns_used + 1;
        match first_failed_job
            .as_ref()
            .filter(|job| job.state == WorkflowJobState::Cancelled)
        {
            Some(job) => {
                journal.append_ci_watch(CiWatchJournalRecord::RerunByJobId {
                    instance: self.config.instance.clone(),
                    run_id: run.id.to_string(),
                    job_id: job.id.to_string(),
                    rerun_number,
                })?;
                self.reruns_used = rerun_number;
                if let Err(error) = provider.rerun_job_by_id(run, job) {
                    return self.rerun_transport_retry(journal, run, error);
                }
            }
            None => {
                journal.append_ci_watch(CiWatchJournalRecord::RerunFailed {
                    instance: self.config.instance.clone(),
                    run_id: run.id.to_string(),
                    rerun_number,
                })?;
                self.reruns_used = rerun_number;
                if let Err(error) = provider.rerun_failed_jobs(run) {
                    return self.rerun_transport_retry(journal, run, error);
                }
            }
        }

        self.record_undecidable(
            journal,
            Some(run.id.to_string()),
            self.config.settle.undecidable(format!(
                "requested rerun {rerun_number} for GitHub workflow run {}",
                run.id
            )),
        )
    }

    fn rerun_transport_retry<J: CiWatchJournal>(
        &self,
        journal: &mut J,
        run: &WorkflowRun,
        error: ProviderError,
    ) -> Result<CiWatchConclusion, CiWatchError> {
        if error.is_transient() {
            return self.record_undecidable(
                journal,
                Some(run.id.to_string()),
                self.transport_retry("requesting a GitHub workflow rerun", &error),
            );
        }
        Err(CiWatchError::Provider(error))
    }

    fn transport_retry(&self, action: &str, error: &ProviderError) -> UndecidableObservation {
        self.config
            .settle
            .undecidable(format!("transport drop while {action}: {error}"))
    }

    fn record_undecidable<J: CiWatchJournal>(
        &self,
        journal: &mut J,
        run_id: Option<String>,
        observation: UndecidableObservation,
    ) -> Result<CiWatchConclusion, CiWatchError> {
        journal.append_ci_watch(CiWatchJournalRecord::Undecidable {
            instance: self.config.instance.clone(),
            run_id,
            observation: observation.clone(),
        })?;
        Ok(CiWatchConclusion::Undecidable(observation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{WorkflowJobId, WorkflowRunId};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct RecordingJournal(Vec<CiWatchJournalRecord>);

    impl CiWatchJournal for RecordingJournal {
        fn append_ci_watch(&mut self, record: CiWatchJournalRecord) -> Result<(), CiWatchError> {
            self.0.push(record);
            Ok(())
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum ProviderCall {
        Capture(WorkflowRunRequest),
        Poll(String),
        FirstFailedJob(String),
        RerunFailed(String),
        RerunByJobId { run_id: String, job_id: String },
    }

    struct FakeProvider {
        captures: VecDeque<Result<WorkflowRunCapture, ProviderError>>,
        polls: VecDeque<Result<WorkflowRunPoll, ProviderError>>,
        jobs: VecDeque<Result<Option<WorkflowJob>, ProviderError>>,
        calls: Vec<ProviderCall>,
    }

    impl FakeProvider {
        fn new(
            captures: impl IntoIterator<Item = Result<WorkflowRunCapture, ProviderError>>,
            polls: impl IntoIterator<Item = Result<WorkflowRunPoll, ProviderError>>,
            jobs: impl IntoIterator<Item = Result<Option<WorkflowJob>, ProviderError>>,
        ) -> Self {
            Self {
                captures: captures.into_iter().collect(),
                polls: polls.into_iter().collect(),
                jobs: jobs.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl GitHubProvider for FakeProvider {
        fn capture_workflow_run(
            &mut self,
            request: &WorkflowRunRequest,
        ) -> Result<WorkflowRunCapture, ProviderError> {
            self.calls.push(ProviderCall::Capture(request.clone()));
            self.captures
                .pop_front()
                .expect("test must script a capture result")
        }

        fn poll_workflow_run(
            &mut self,
            run: &WorkflowRun,
        ) -> Result<WorkflowRunPoll, ProviderError> {
            self.calls.push(ProviderCall::Poll(run.id.to_string()));
            self.polls
                .pop_front()
                .expect("test must script a poll result")
        }

        fn first_failed_job(
            &mut self,
            run: &WorkflowRun,
        ) -> Result<Option<WorkflowJob>, ProviderError> {
            self.calls
                .push(ProviderCall::FirstFailedJob(run.id.to_string()));
            self.jobs
                .pop_front()
                .expect("test must script a diagnostic job result")
        }

        fn rerun_failed_jobs(&mut self, run: &WorkflowRun) -> Result<(), ProviderError> {
            self.calls
                .push(ProviderCall::RerunFailed(run.id.to_string()));
            Ok(())
        }

        fn rerun_job_by_id(
            &mut self,
            run: &WorkflowRun,
            job: &WorkflowJob,
        ) -> Result<(), ProviderError> {
            self.calls.push(ProviderCall::RerunByJobId {
                run_id: run.id.to_string(),
                job_id: job.id.to_string(),
            });
            Ok(())
        }
    }

    fn config(
        instance: &str,
        workflow: &str,
        selector: &str,
        retry_after_ms: u64,
        settle_deadline_ms: u64,
        rerun_budget: u32,
    ) -> CiWatchConfig {
        CiWatchConfig::new(
            PhaseInstanceId::new(instance),
            GitHubRepository::new("cortexkit/example").unwrap(),
            workflow,
            selector,
            SettleParameters::new(retry_after_ms, settle_deadline_ms).unwrap(),
            rerun_budget,
        )
        .unwrap()
    }

    fn run(config: &CiWatchConfig, id: &str) -> WorkflowRun {
        WorkflowRun {
            repository: config.repository.clone(),
            id: WorkflowRunId::new(id).unwrap(),
            workflow: config.workflow.clone(),
            selector: config.selector.clone(),
            settle: config.settle,
        }
    }

    fn failure(id: &str) -> WorkflowRunPoll {
        WorkflowRunPoll::Failed(MatchingEvidence {
            reference: format!("run:{id}"),
            identity: "commit-a".to_owned(),
        })
    }

    fn job(id: &str, state: WorkflowJobState) -> WorkflowJob {
        WorkflowJob {
            id: WorkflowJobId::new(id).unwrap(),
            name: format!("job-{id}"),
            state,
        }
    }

    #[test]
    fn instances_keep_independent_ids_selectors_runs_settle_and_rerun_budgets() {
        let first_config = config("tests-before-tag", "Tests", "commit:commit-a", 50, 500, 1);
        let second_config = config("release-after-tag", "Release", "tag:v1.2.3", 75, 900, 2);
        let first_run = run(&first_config, "101");
        let second_run = run(&second_config, "202");
        let mut provider = FakeProvider::new(
            [
                Ok(WorkflowRunCapture::Captured(first_run.clone())),
                Ok(WorkflowRunCapture::Captured(second_run.clone())),
            ],
            [Ok(failure("101")), Ok(failure("202")), Ok(failure("101"))],
            [
                Ok(Some(job("301", WorkflowJobState::Failed))),
                Ok(Some(job("302", WorkflowJobState::Cancelled))),
                Ok(Some(job("301", WorkflowJobState::Failed))),
            ],
        );
        let mut journal = RecordingJournal::default();
        let mut first = CiWatch::new(first_config.clone());
        let mut second = CiWatch::new(second_config.clone());

        assert!(matches!(
            first.step(&mut provider, &mut journal).unwrap(),
            CiWatchConclusion::Undecidable(_)
        ));
        assert!(matches!(
            second.step(&mut provider, &mut journal).unwrap(),
            CiWatchConclusion::Undecidable(_)
        ));
        let exhausted = first.step(&mut provider, &mut journal).unwrap();

        assert_eq!(first.run(), Some(&first_run));
        assert_eq!(second.run(), Some(&second_run));
        assert_eq!(first.config().selector, "commit:commit-a");
        assert_eq!(second.config().selector, "tag:v1.2.3");
        assert_eq!(first.config().settle.settle_deadline_ms, 500);
        assert_eq!(second.config().settle.settle_deadline_ms, 900);
        assert_eq!(first.config().rerun_budget, 1);
        assert_eq!(second.config().rerun_budget, 2);
        assert_eq!(first.reruns_used(), 1);
        assert_eq!(second.reruns_used(), 1);
        assert!(matches!(exhausted, CiWatchConclusion::Failed { .. }));
        assert_eq!(
            provider.calls,
            vec![
                ProviderCall::Capture(first_config.run_request().unwrap()),
                ProviderCall::Poll("101".to_owned()),
                ProviderCall::FirstFailedJob("101".to_owned()),
                ProviderCall::RerunFailed("101".to_owned()),
                ProviderCall::Capture(second_config.run_request().unwrap()),
                ProviderCall::Poll("202".to_owned()),
                ProviderCall::FirstFailedJob("202".to_owned()),
                ProviderCall::RerunByJobId {
                    run_id: "202".to_owned(),
                    job_id: "302".to_owned(),
                },
                ProviderCall::Poll("101".to_owned()),
                ProviderCall::FirstFailedJob("101".to_owned()),
            ]
        );
        assert!(journal.0.iter().any(|record| matches!(
            record,
            CiWatchJournalRecord::RerunFailed {
                instance,
                run_id,
                rerun_number: 1,
            } if instance.as_str() == "tests-before-tag" && run_id == "101"
        )));
        assert!(journal.0.iter().any(|record| matches!(
            record,
            CiWatchJournalRecord::RerunByJobId {
                instance,
                run_id,
                job_id,
                rerun_number: 1,
            } if instance.as_str() == "release-after-tag" && run_id == "202" && job_id == "302"
        )));
    }

    #[test]
    fn transport_drop_is_retry_guidance_not_a_workflow_failure() {
        let config = config("tests", "Tests", "commit:commit-a", 25, 200, 0);
        let mut provider =
            FakeProvider::new([Err(ProviderError::transport("connection reset"))], [], []);
        let mut journal = RecordingJournal::default();
        let mut watch = CiWatch::new(config);

        let conclusion = watch.step(&mut provider, &mut journal).unwrap();

        assert!(matches!(conclusion, CiWatchConclusion::Undecidable(_)));
        assert!(watch.run().is_none());
        assert!(matches!(
            journal.0.as_slice(),
            [CiWatchJournalRecord::Undecidable { .. }]
        ));
    }
}
