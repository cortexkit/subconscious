//! GitHub v1 provider seam and completion-probe vocabulary.
//!
//! GitHub command execution is separated from provider semantics so a direct
//! human invocation and a governed agent-seat shim issue the same logical
//! requests. The provider never uses a shell, so repository and workflow
//! values remain individual command arguments.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{fmt, process::Command};
use thiserror::Error;

/// GitHub's repository identifier in `owner/name` form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitHubRepository(String);

impl GitHubRepository {
    /// Creates a repository identifier after rejecting an empty value.
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderError::invalid_response(
                "GitHub repository identifier cannot be empty",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the identifier in the form expected by GitHub CLI commands.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitHubRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque GitHub Actions workflow-run identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowRunId(String);

impl WorkflowRunId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderError::invalid_response(
                "GitHub workflow run identifier cannot be empty",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque GitHub Actions job identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowJobId(String);

impl WorkflowJobId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderError::invalid_response(
                "GitHub workflow job identifier cannot be empty",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowJobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A declared period in which missing provider evidence may still propagate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettleParameters {
    /// Earliest delay before another observation should be attempted.
    pub retry_after_ms: u64,
    /// Absolute millisecond deadline after which absence is authoritative.
    pub settle_deadline_ms: u64,
}

impl SettleParameters {
    /// Builds parameters that always leave a positive retry interval.
    pub fn new(retry_after_ms: u64, settle_deadline_ms: u64) -> Result<Self, ProviderError> {
        if retry_after_ms == 0 {
            return Err(ProviderError::invalid_response(
                "settle retry_after_ms must be greater than zero",
            ));
        }
        Ok(Self {
            retry_after_ms,
            settle_deadline_ms,
        })
    }

    /// Builds retry guidance that preserves this instance's declared deadline.
    pub fn undecidable(&self, reason: impl Into<String>) -> UndecidableObservation {
        UndecidableObservation {
            reason: reason.into(),
            retry_after_ms: self.retry_after_ms,
            settle_deadline_ms: self.settle_deadline_ms,
        }
    }
}

/// Evidence that proves an observed effect belongs to the intended identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatchingEvidence {
    /// Provider-specific stable reference for the effect.
    pub reference: String,
    /// Commit, artifact, or release identity reported by the provider.
    pub identity: String,
}

/// Evidence that an effect is authoritatively absent after its settle budget.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthoritativeAbsence {
    /// Provider endpoint or query that found no matching effect.
    pub reference: String,
    /// Millisecond instant at which the authoritative absence was observed.
    pub observed_at_ms: u64,
}

/// Retry guidance returned when a provider cannot yet decide an observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UndecidableObservation {
    pub reason: String,
    pub retry_after_ms: u64,
    pub settle_deadline_ms: u64,
}

/// A completion observation with no lossy absent-or-present collapse.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompletionObservation {
    /// The intended effect exists and carries matching identity evidence.
    Present(MatchingEvidence),
    /// The intended effect is authoritatively absent after settling.
    Absent(AuthoritativeAbsence),
    /// The provider must be queried again before a conclusion is safe.
    Undecidable(UndecidableObservation),
}

/// Raw result from a provider lookup before release-machine identity validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawCompletionProbe {
    /// The provider found an effect and reported its identity.
    Found(MatchingEvidence),
    /// The provider found no effect at its authoritative query endpoint.
    Missing { reference: String },
}

/// Stable reasons why a provider observation must stop execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRefusalCode {
    /// Provider evidence names an effect that belongs to another identity.
    ContradictoryIdentity,
}

/// A typed provider refusal that callers can render without parsing prose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRefusal {
    pub code: ProviderRefusalCode,
    pub expected_identity: String,
    pub observed_identity: String,
    pub reference: String,
}

/// Failures emitted by the GitHub provider seam.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProviderError {
    #[error("provider transport error: {0}")]
    Transport(String),
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error(
        "provider refusal {code:?}: expected identity `{expected_identity}` but `{reference}` reports `{observed_identity}`"
    )]
    Refusal {
        code: ProviderRefusalCode,
        expected_identity: String,
        observed_identity: String,
        reference: String,
    },
}

impl ProviderError {
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::InvalidResponse(message.into())
    }

    /// Returns true only for failures that may be retried without changing a verdict.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    pub fn refusal(&self) -> Option<ProviderRefusal> {
        match self {
            Self::Refusal {
                code,
                expected_identity,
                observed_identity,
                reference,
            } => Some(ProviderRefusal {
                code: *code,
                expected_identity: expected_identity.clone(),
                observed_identity: observed_identity.clone(),
                reference: reference.clone(),
            }),
            Self::Transport(_) | Self::InvalidResponse(_) => None,
        }
    }
}

/// Preserves propagation-sensitive absence until the declared deadline expires.
pub fn classify_completion_probe(
    expected_identity: &str,
    observed_at_ms: u64,
    settle: SettleParameters,
    result: RawCompletionProbe,
) -> Result<CompletionObservation, ProviderError> {
    match result {
        RawCompletionProbe::Found(evidence) if evidence.identity == expected_identity => {
            Ok(CompletionObservation::Present(evidence))
        }
        RawCompletionProbe::Found(evidence) => Err(ProviderError::Refusal {
            code: ProviderRefusalCode::ContradictoryIdentity,
            expected_identity: expected_identity.to_owned(),
            observed_identity: evidence.identity,
            reference: evidence.reference,
        }),
        RawCompletionProbe::Missing { reference } if observed_at_ms < settle.settle_deadline_ms => {
            Ok(CompletionObservation::Undecidable(settle.undecidable(
                format!(
                    "no matching effect is visible from `{reference}` before its settle deadline"
                ),
            )))
        }
        RawCompletionProbe::Missing { reference } => {
            Ok(CompletionObservation::Absent(AuthoritativeAbsence {
                reference,
                observed_at_ms,
            }))
        }
    }
}

/// Selection criteria for the one GitHub Actions run owned by a watch instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunRequest {
    pub repository: GitHubRepository,
    pub workflow: String,
    /// `commit:<sha>` or `tag:<name>`; it is retained verbatim in the journal.
    pub selector: String,
    pub settle: SettleParameters,
}

impl WorkflowRunRequest {
    pub fn new(
        repository: GitHubRepository,
        workflow: impl Into<String>,
        selector: impl Into<String>,
        settle: SettleParameters,
    ) -> Result<Self, ProviderError> {
        let workflow = workflow.into();
        let selector = selector.into();
        if workflow.trim().is_empty() || selector.trim().is_empty() {
            return Err(ProviderError::invalid_response(
                "workflow run requests require non-empty workflow and selector",
            ));
        }
        Ok(Self {
            repository,
            workflow,
            selector,
            settle,
        })
    }
}

/// The run captured at ci-watch entry and used for every later poll.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub repository: GitHubRepository,
    pub id: WorkflowRunId,
    pub workflow: String,
    pub selector: String,
    pub settle: SettleParameters,
}

/// Result of attempting to capture a workflow run at phase entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowRunCapture {
    Captured(WorkflowRun),
    Undecidable(UndecidableObservation),
}

/// A final or in-progress state for the captured workflow run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowRunPoll {
    Succeeded(MatchingEvidence),
    Failed(MatchingEvidence),
    Undecidable(UndecidableObservation),
}

/// State of a job returned for failure diagnostics and rerun selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkflowJobState {
    Failed,
    Cancelled,
}

/// The first relevant failed job in a decided failed workflow run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub id: WorkflowJobId,
    pub name: String,
    pub state: WorkflowJobState,
}

/// Logical GitHub operations used by ci-watch and replaceable by hermetic fakes.
pub trait GitHubProvider {
    fn capture_workflow_run(
        &mut self,
        request: &WorkflowRunRequest,
    ) -> Result<WorkflowRunCapture, ProviderError>;

    fn poll_workflow_run(&mut self, run: &WorkflowRun) -> Result<WorkflowRunPoll, ProviderError>;

    /// Returns the first failed or cancelled job after a failed run is decided.
    fn first_failed_job(&mut self, run: &WorkflowRun)
        -> Result<Option<WorkflowJob>, ProviderError>;

    fn rerun_failed_jobs(&mut self, run: &WorkflowRun) -> Result<(), ProviderError>;

    fn rerun_job_by_id(
        &mut self,
        run: &WorkflowRun,
        job: &WorkflowJob,
    ) -> Result<(), ProviderError>;
}

/// One shell-free invocation of the GitHub CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubCommand {
    pub arguments: Vec<String>,
}

impl GitHubCommand {
    fn new(arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            arguments: arguments.into_iter().map(Into::into).collect(),
        }
    }
}

/// Output returned by a GitHub command executor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitHubCommandOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Execution seam for direct `gh` calls and governed agent-seat shims.
pub trait GitHubCommandExecutor {
    fn execute(&mut self, command: &GitHubCommand) -> Result<GitHubCommandOutput, ProviderError>;
}

/// Direct human execution through the local `gh` binary.
#[derive(Clone, Debug, Default)]
pub struct DirectGitHubCommandExecutor;

impl GitHubCommandExecutor for DirectGitHubCommandExecutor {
    fn execute(&mut self, command: &GitHubCommand) -> Result<GitHubCommandOutput, ProviderError> {
        let output = Command::new("gh")
            .args(&command.arguments)
            .output()
            .map_err(|error| ProviderError::transport(format!("could not start gh: {error}")))?;
        if !output.status.success() {
            return Err(ProviderError::transport(format!(
                "gh {:?} exited with {}: {}",
                command.arguments,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(GitHubCommandOutput {
            stdout: String::from_utf8(output.stdout).map_err(|error| {
                ProviderError::invalid_response(format!("gh stdout was not UTF-8: {error}"))
            })?,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Agent-seat adapter for an external governed command shim.
pub trait GovernedGitHubShim {
    fn execute_github(
        &mut self,
        command: &GitHubCommand,
    ) -> Result<GitHubCommandOutput, ProviderError>;
}

/// Makes a governed shim interchangeable with direct human execution.
#[derive(Clone, Debug)]
pub struct GovernedGitHubCommandExecutor<S> {
    shim: S,
}

impl<S> GovernedGitHubCommandExecutor<S> {
    pub fn new(shim: S) -> Self {
        Self { shim }
    }

    pub fn into_inner(self) -> S {
        self.shim
    }
}

impl<S: GovernedGitHubShim> GitHubCommandExecutor for GovernedGitHubCommandExecutor<S> {
    fn execute(&mut self, command: &GitHubCommand) -> Result<GitHubCommandOutput, ProviderError> {
        self.shim.execute_github(command)
    }
}

/// GitHub-v1 implementation whose logical semantics are independent of command transport.
#[derive(Clone, Debug)]
pub struct GitHubCliProvider<E> {
    executor: E,
}

impl<E> GitHubCliProvider<E> {
    pub fn new(executor: E) -> Self {
        Self { executor }
    }

    pub fn into_executor(self) -> E {
        self.executor
    }
}

impl<E: GitHubCommandExecutor> GitHubCliProvider<E> {
    fn execute_json<T: DeserializeOwned>(
        &mut self,
        arguments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<T, ProviderError> {
        let output = self.executor.execute(&GitHubCommand::new(arguments))?;
        serde_json::from_str(&output.stdout).map_err(|error| {
            ProviderError::invalid_response(format!("gh returned invalid JSON: {error}"))
        })
    }

    fn execute_empty(
        &mut self,
        arguments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(), ProviderError> {
        self.executor.execute(&GitHubCommand::new(arguments))?;
        Ok(())
    }
}

impl<E: GitHubCommandExecutor> GitHubProvider for GitHubCliProvider<E> {
    fn capture_workflow_run(
        &mut self,
        request: &WorkflowRunRequest,
    ) -> Result<WorkflowRunCapture, ProviderError> {
        let runs: Vec<GitHubRunJson> = self.execute_json([
            "run".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            request.repository.to_string(),
            "--workflow".to_owned(),
            request.workflow.clone(),
            "--json".to_owned(),
            "databaseId,headSha,headBranch".to_owned(),
            "--limit".to_owned(),
            "100".to_owned(),
        ])?;
        let run = runs
            .into_iter()
            .find(|run| selector_matches(&request.selector, run))
            .map(|run| WorkflowRunId::new(run.database_id.to_string()))
            .transpose()?;
        Ok(match run {
            Some(id) => WorkflowRunCapture::Captured(WorkflowRun {
                repository: request.repository.clone(),
                id,
                workflow: request.workflow.clone(),
                selector: request.selector.clone(),
                settle: request.settle,
            }),
            None => WorkflowRunCapture::Undecidable(request.settle.undecidable(format!(
                "no {} run matches selector `{}` yet",
                request.workflow, request.selector
            ))),
        })
    }

    fn poll_workflow_run(&mut self, run: &WorkflowRun) -> Result<WorkflowRunPoll, ProviderError> {
        let status: GitHubRunStatusJson = self.execute_json([
            "run".to_owned(),
            "view".to_owned(),
            run.id.to_string(),
            "--repo".to_owned(),
            run.repository.to_string(),
            "--json".to_owned(),
            "databaseId,status,conclusion,headSha".to_owned(),
        ])?;
        let evidence = MatchingEvidence {
            reference: format!("github-actions-run:{}", status.database_id),
            identity: status.head_sha,
        };
        match (status.status.as_str(), status.conclusion.as_deref()) {
            ("completed", Some("success")) => Ok(WorkflowRunPoll::Succeeded(evidence)),
            ("completed", _) => Ok(WorkflowRunPoll::Failed(evidence)),
            _ => Ok(WorkflowRunPoll::Undecidable(run.settle.undecidable(
                format!("GitHub workflow run {} is still {}", run.id, status.status),
            ))),
        }
    }

    fn first_failed_job(
        &mut self,
        run: &WorkflowRun,
    ) -> Result<Option<WorkflowJob>, ProviderError> {
        let result: GitHubJobsJson = self.execute_json([
            "run".to_owned(),
            "view".to_owned(),
            run.id.to_string(),
            "--repo".to_owned(),
            run.repository.to_string(),
            "--json".to_owned(),
            "jobs".to_owned(),
        ])?;
        let mut cancelled = None;
        for job in result.jobs {
            let state = match job.conclusion.as_deref() {
                Some("failure") => WorkflowJobState::Failed,
                Some("cancelled") => WorkflowJobState::Cancelled,
                _ => continue,
            };
            let job = WorkflowJob {
                id: WorkflowJobId::new(job.database_id.to_string())?,
                name: job.name,
                state,
            };
            if job.state == WorkflowJobState::Failed {
                return Ok(Some(job));
            }
            cancelled = Some(job);
        }
        Ok(cancelled)
    }

    fn rerun_failed_jobs(&mut self, run: &WorkflowRun) -> Result<(), ProviderError> {
        self.execute_empty([
            "run".to_owned(),
            "rerun".to_owned(),
            run.id.to_string(),
            "--repo".to_owned(),
            run.repository.to_string(),
            "--failed".to_owned(),
        ])
    }

    fn rerun_job_by_id(
        &mut self,
        run: &WorkflowRun,
        job: &WorkflowJob,
    ) -> Result<(), ProviderError> {
        self.execute_empty([
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!(
                "repos/{}/actions/jobs/{}/rerun",
                run.repository,
                job.id.as_str()
            ),
        ])
    }
}

fn selector_matches(selector: &str, run: &GitHubRunJson) -> bool {
    let Some((kind, value)) = selector.split_once(':') else {
        return false;
    };
    match kind {
        "commit" => run.head_sha == value,
        "tag" => run.head_branch == value,
        _ => false,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitHubRunJson {
    database_id: u64,
    head_sha: String,
    head_branch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitHubRunStatusJson {
    database_id: u64,
    status: String,
    conclusion: Option<String>,
    head_sha: String,
}

#[derive(Deserialize)]
struct GitHubJobsJson {
    jobs: Vec<GitHubJobJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitHubJobJson {
    database_id: u64,
    name: String,
    conclusion: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_evidence_stays_undecidable_until_the_settle_deadline() {
        let settle = SettleParameters::new(250, 5_000).unwrap();
        let before = classify_completion_probe(
            "commit-a",
            4_999,
            settle,
            RawCompletionProbe::Missing {
                reference: "releases/v1".to_owned(),
            },
        )
        .unwrap();
        let after = classify_completion_probe(
            "commit-a",
            5_000,
            settle,
            RawCompletionProbe::Missing {
                reference: "releases/v1".to_owned(),
            },
        )
        .unwrap();

        assert!(matches!(before, CompletionObservation::Undecidable(_)));
        assert!(matches!(after, CompletionObservation::Absent(_)));
    }

    #[test]
    fn contradictory_provider_identity_is_a_typed_refusal() {
        let error = classify_completion_probe(
            "commit-a",
            0,
            SettleParameters::new(1, 0).unwrap(),
            RawCompletionProbe::Found(MatchingEvidence {
                reference: "github-release:1".to_owned(),
                identity: "commit-b".to_owned(),
            }),
        )
        .unwrap_err();

        assert_eq!(
            error.refusal().unwrap().code,
            ProviderRefusalCode::ContradictoryIdentity
        );
    }
}
