//! Stable adopter-case acceptance matrix for the release machine.
//!
//! Every declaration is copied into a Git repository minted for the test. Public
//! effects use recording seams, so this matrix never contacts a release provider.

#[path = "../support/mod.rs"]
mod support;

use cortexkit_release::executor;
use cortexkit_release::{
    approval::{build_approval_subject, ApprovalError, ApprovalStore, ApprovalSubject},
    declaration::{parse, ParsedDeclaration},
    lease::LeaseStore,
    orchestrator::{
        reconcile_effect_unfenced_for_tests as reconcile_effect, EffectOutcome,
        FirstPublicTriggerGate, OrchestrationError, Orchestrator, PrecheckRefusalCode,
    },
    phases::{
        ci_watch::{
            CiWatch, CiWatchConclusion, CiWatchConfig, CiWatchError, CiWatchJournal,
            CiWatchJournalRecord,
        },
        precheck::PrecheckRunner,
    },
    plan::{build_dry_run_plan, FinalizedArtifact, ReleaseIdentity, ReleasePlan},
    provider::{
        GitHubProvider, GitHubRepository, MatchingEvidence, ProviderError, SettleParameters,
        WorkflowJob, WorkflowJobId, WorkflowJobState, WorkflowRun, WorkflowRunCapture,
        WorkflowRunId, WorkflowRunPoll, WorkflowRunRequest,
    },
    state::{JournalRecord, JournalStore, TrainJournalIdentity, TrainTerminalState},
    ApprovalSubject as DurableApprovalSubject, ApprovalToken, ArtifactDigest, ArtifactId,
    CompletionProbe, EffectRequest, IrreversibleExecutor, ProbeEvidence, ProbeResult, RepositoryId,
    SeamError,
};
use std::{
    collections::VecDeque,
    fs,
    io::{ErrorKind, Write},
    path::PathBuf,
};
use tempfile::TempDir;

const SYNTHETIC_E2E_01: &str = "synthetic-e2e-01";
const MC_SAGA_01: &str = "mc-saga-01";
const MC_SAGA_02: &str = "mc-saga-02";
const MC_SAGA_03: &str = "mc-saga-03";
const MC_SAGA_04: &str = "mc-saga-04";
const MC_SAGA_05: &str = "mc-saga-05";
const MC_SAGA_06: &str = "mc-saga-06";
const MC_SAGA_07: &str = "mc-saga-07";
const MC_SAGA_08: &str = "mc-saga-08";
const MC_SAGA_09: &str = "mc-saga-09";
const MC_SAGA_10: &str = "mc-saga-10";
const MC_SAGA_11: &str = "mc-saga-11";
const ALF_NOTAG_01: &str = "alf-notag-01";
const AFT_CIW_01: &str = "aft-ciw-01";
const AFT_CIW_02: &str = "aft-ciw-02";

const SYNTHETIC_FIXTURE: &str = include_str!("../data/adopters/synthetic-e2e-01.release.jsonc");
const MC_SAGA_01_FIXTURE: &str = include_str!("../data/adopters/mc-saga-01.release.jsonc");
const MC_SAGA_02_FIXTURE: &str = include_str!("../data/adopters/mc-saga-02.release.jsonc");
const MC_SAGA_03_FIXTURE: &str = include_str!("../data/adopters/mc-saga-03.release.jsonc");
const MC_SAGA_04_FIXTURE: &str = include_str!("../data/adopters/mc-saga-04.release.jsonc");
const MC_SAGA_05_FIXTURE: &str = include_str!("../data/adopters/mc-saga-05.release.jsonc");
const MC_SAGA_06_FIXTURE: &str = include_str!("../data/adopters/mc-saga-06.release.jsonc");
const MC_SAGA_07_FIXTURE: &str = include_str!("../data/adopters/mc-saga-07.release.jsonc");
const MC_SAGA_08_FIXTURE: &str = include_str!("../data/adopters/mc-saga-08.release.jsonc");
const MC_SAGA_09_FIXTURE: &str = include_str!("../data/adopters/mc-saga-09.release.jsonc");
const MC_SAGA_10_FIXTURE: &str = include_str!("../data/adopters/mc-saga-10.release.jsonc");
const MC_SAGA_11_FIXTURE: &str = include_str!("../data/adopters/mc-saga-11.release.jsonc");
const ALF_NOTAG_FIXTURE: &str = include_str!("../data/adopters/alf-notag-01.release.jsonc");
const AFT_CIW_01_FIXTURE: &str =
    include_str!("../data/adopters/aft-ciw-01-pre-tag-tests.release.jsonc");
const AFT_CIW_02_FIXTURE: &str =
    include_str!("../data/adopters/aft-ciw-02-post-tag-release.release.jsonc");

fn parsed_case(case_id: &str, fixture: &str) -> (support::MintedRepo, ParsedDeclaration) {
    parsed_case_with_shape(case_id, support::RepositoryShape::Valid, fixture)
}

fn parsed_case_with_shape(
    case_id: &str,
    shape: support::RepositoryShape,
    fixture: &str,
) -> (support::MintedRepo, ParsedDeclaration) {
    let repository = support::MintedRepo::mint_with_declaration(shape, fixture)
        .unwrap_or_else(|error| panic!("{case_id}: could not mint hermetic repository: {error}"));
    let source = fs::read_to_string(repository.declaration_path())
        .unwrap_or_else(|error| panic!("{case_id}: could not read minted declaration: {error}"));
    let declaration =
        parse(&source).unwrap_or_else(|error| panic!("{case_id}: declaration must parse: {error}"));
    (repository, declaration)
}

fn planned_case(
    case_id: &str,
    fixture: &str,
    artifact_material: &[(&str, &str)],
) -> (support::MintedRepo, ParsedDeclaration, ReleasePlan) {
    planned_case_with_shape(
        case_id,
        support::RepositoryShape::Valid,
        fixture,
        artifact_material,
    )
}

fn planned_case_with_shape(
    case_id: &str,
    shape: support::RepositoryShape,
    fixture: &str,
    artifact_material: &[(&str, &str)],
) -> (support::MintedRepo, ParsedDeclaration, ReleasePlan) {
    let (repository, declaration) = parsed_case_with_shape(case_id, shape, fixture);
    let artifacts = artifact_material
        .iter()
        .map(|(artifact, identity)| FinalizedArtifact {
            artifact: ArtifactId::new(*artifact),
            identity: (*identity).to_owned(),
            bytes: format!("finalized {case_id} {artifact}").into_bytes(),
        })
        .collect::<Vec<_>>();
    let train = &declaration.declaration.trains[0];
    let plan = build_dry_run_plan(
        RepositoryId::new(format!("adopter-{case_id}")),
        &declaration,
        &train.id,
        &artifacts,
    )
    .unwrap_or_else(|error| panic!("{case_id}: declaration must produce a plan: {error}"));
    (repository, declaration, plan)
}

fn journal_for(
    case_id: &str,
    declaration: &ParsedDeclaration,
    plan: &ReleasePlan,
) -> (TempDir, JournalStore) {
    let root = tempfile::tempdir().unwrap();
    let identity = TrainJournalIdentity::new(
        plan.repository.clone(),
        plan.train.clone(),
        format!("{case_id}-runtime"),
    )
    .unwrap();
    let journal = JournalStore::new(root.path(), identity).unwrap();
    journal.pin_declaration(declaration).unwrap();
    (root, journal)
}

struct UnexpectedGate;

impl FirstPublicTriggerGate for UnexpectedGate {
    fn confirm(&mut self, _: &ApprovalSubject) -> Result<ApprovalToken, SeamError> {
        Err(SeamError::new(
            "precheck refusal test reached the public trigger",
        ))
    }
}

fn execute_prechecks(
    case_id: &str,
    repository: &support::MintedRepo,
    declaration: &ParsedDeclaration,
    plan: &ReleasePlan,
) -> (
    Result<Vec<EffectOutcome>, OrchestrationError>,
    Vec<JournalRecord>,
    usize,
) {
    execute_prechecks_after(case_id, repository, declaration, plan, |_, _| {})
}

fn execute_prechecks_after(
    case_id: &str,
    repository: &support::MintedRepo,
    declaration: &ParsedDeclaration,
    plan: &ReleasePlan,
    setup: impl FnOnce(&std::path::Path, &JournalStore),
) -> (
    Result<Vec<EffectOutcome>, OrchestrationError>,
    Vec<JournalRecord>,
    usize,
) {
    let (state_home, journal) = journal_for(case_id, declaration, plan);
    setup(state_home.path(), &journal);
    let approvals = ApprovalStore::new(
        state_home.path(),
        TrainJournalIdentity::new(
            plan.repository.clone(),
            plan.train.clone(),
            format!("{case_id}-runtime"),
        )
        .unwrap(),
    )
    .unwrap();
    let leases = LeaseStore::new(state_home.path()).unwrap();
    let mut runner = PrecheckRunner::new(repository.path(), &journal);
    let mut gate = UnexpectedGate;
    let mut probe = ScriptedProbe::new([]);
    let mut executor = CountingExecutor::default();
    let result = Orchestrator::default().execute(
        plan,
        &leases,
        &journal,
        &approvals,
        &mut runner,
        &mut gate,
        &mut probe,
        &mut executor,
    );
    (result, journal.read_journal().unwrap(), executor.calls)
}

fn assert_precheck_refusal(
    result: &Result<Vec<EffectOutcome>, OrchestrationError>,
    records: &[JournalRecord],
    code: PrecheckRefusalCode,
) {
    assert!(
        matches!(
            result,
            Err(OrchestrationError::PrecheckRefusal { code: observed, .. }) if *observed == code
        ),
        "unexpected precheck result: {result:?}"
    );
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::Refused { reason, .. } if reason.contains(&code.to_string())
    )));
}

fn durable_subject(subject: &ApprovalSubject) -> DurableApprovalSubject {
    DurableApprovalSubject {
        repository: subject.repository.clone(),
        train: subject.train.clone(),
        intended_commit: subject.intended_commit.clone(),
        declaration_digest: subject.declaration_digest.clone(),
        artifact_digests: subject
            .artifacts
            .iter()
            .map(|artifact| ArtifactDigest {
                artifact: artifact.artifact.clone(),
                digest: artifact.digest.clone(),
            })
            .collect(),
        public_effects: subject
            .public_effects
            .iter()
            .map(|effect| effect.operation.clone())
            .collect(),
    }
}

fn evidence(identity: impl Into<String>) -> ProbeEvidence {
    let identity = identity.into();
    ProbeEvidence {
        reference: format!("recording-provider/{identity}"),
        identity,
    }
}

struct ScriptedProbe {
    outcomes: VecDeque<ProbeResult>,
    calls: usize,
}

impl ScriptedProbe {
    fn new(outcomes: impl IntoIterator<Item = ProbeResult>) -> Self {
        Self {
            outcomes: outcomes.into_iter().collect(),
            calls: 0,
        }
    }
}

impl CompletionProbe for ScriptedProbe {
    fn probe(&mut self, _: &EffectRequest) -> Result<ProbeResult, SeamError> {
        self.calls += 1;
        self.outcomes
            .pop_front()
            .ok_or_else(|| SeamError::new("test did not script enough done-probe results"))
    }
}

#[derive(Default)]
struct CountingExecutor {
    calls: usize,
}

impl IrreversibleExecutor for CountingExecutor {
    fn execute(&mut self, _: &executor::AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        Err(SeamError::new(
            "recording executor must not receive an irreversible re-fire",
        ))
    }
}

struct FileEffectProbe {
    effect_path: PathBuf,
    absent_evidence: ProbeEvidence,
    calls: usize,
}

impl FileEffectProbe {
    fn new(effect_path: PathBuf, absent_evidence: ProbeEvidence) -> Self {
        Self {
            effect_path,
            absent_evidence,
            calls: 0,
        }
    }
}

impl CompletionProbe for FileEffectProbe {
    fn probe(&mut self, _: &EffectRequest) -> Result<ProbeResult, SeamError> {
        self.calls += 1;
        match fs::read_to_string(&self.effect_path) {
            Ok(record) => {
                let (reference, identity) = record.split_once('\n').ok_or_else(|| {
                    SeamError::new(format!(
                        "durable public-effect record {} is incomplete",
                        self.effect_path.display()
                    ))
                })?;
                Ok(ProbeResult::Present(ProbeEvidence {
                    reference: reference.to_owned(),
                    identity: identity.to_owned(),
                }))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                Ok(ProbeResult::Absent(self.absent_evidence.clone()))
            }
            Err(error) => Err(SeamError::new(format!(
                "could not read durable public-effect record {}: {error}",
                self.effect_path.display()
            ))),
        }
    }
}

struct InterruptAfterRecordedEffect {
    effect_path: PathBuf,
    effect_evidence: ProbeEvidence,
    calls: usize,
}

impl InterruptAfterRecordedEffect {
    fn new(effect_path: PathBuf, effect_evidence: ProbeEvidence) -> Self {
        Self {
            effect_path,
            effect_evidence,
            calls: 0,
        }
    }
}

impl IrreversibleExecutor for InterruptAfterRecordedEffect {
    fn execute(&mut self, _: &executor::AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        fs::create_dir_all(
            self.effect_path
                .parent()
                .expect("public-effect record path has a parent"),
        )
        .map_err(|error| {
            SeamError::new(format!(
                "could not create durable public-effect directory: {error}"
            ))
        })?;
        let mut record = fs::File::create(&self.effect_path).map_err(|error| {
            SeamError::new(format!(
                "could not create durable public-effect record {}: {error}",
                self.effect_path.display()
            ))
        })?;
        write!(
            record,
            "{}\n{}",
            self.effect_evidence.reference, self.effect_evidence.identity
        )
        .map_err(|error| SeamError::new(format!("could not write public effect: {error}")))?;
        record
            .sync_all()
            .map_err(|error| SeamError::new(format!("could not sync public effect: {error}")))?;
        Err(SeamError::new(
            "interrupted after recording durable public effect before completion append",
        ))
    }
}

#[derive(Default)]
struct RecordingCiJournal(Vec<CiWatchJournalRecord>);

impl CiWatchJournal for RecordingCiJournal {
    fn append_ci_watch(&mut self, record: CiWatchJournalRecord) -> Result<(), CiWatchError> {
        self.0.push(record);
        Ok(())
    }
}

struct ScriptedGitHubProvider {
    captures: VecDeque<Result<WorkflowRunCapture, ProviderError>>,
    polls: VecDeque<Result<WorkflowRunPoll, ProviderError>>,
    jobs: VecDeque<Result<Option<WorkflowJob>, ProviderError>>,
    rerun_failed_calls: Vec<String>,
    rerun_job_calls: Vec<(String, String)>,
}

impl ScriptedGitHubProvider {
    fn new(
        captures: impl IntoIterator<Item = Result<WorkflowRunCapture, ProviderError>>,
        polls: impl IntoIterator<Item = Result<WorkflowRunPoll, ProviderError>>,
        jobs: impl IntoIterator<Item = Result<Option<WorkflowJob>, ProviderError>>,
    ) -> Self {
        Self {
            captures: captures.into_iter().collect(),
            polls: polls.into_iter().collect(),
            jobs: jobs.into_iter().collect(),
            rerun_failed_calls: Vec::new(),
            rerun_job_calls: Vec::new(),
        }
    }
}

impl GitHubProvider for ScriptedGitHubProvider {
    fn capture_workflow_run(
        &mut self,
        _: &WorkflowRunRequest,
    ) -> Result<WorkflowRunCapture, ProviderError> {
        self.captures
            .pop_front()
            .expect("test did not script enough workflow captures")
    }

    fn poll_workflow_run(&mut self, _: &WorkflowRun) -> Result<WorkflowRunPoll, ProviderError> {
        self.polls
            .pop_front()
            .expect("test did not script enough workflow polls")
    }

    fn first_failed_job(&mut self, _: &WorkflowRun) -> Result<Option<WorkflowJob>, ProviderError> {
        self.jobs
            .pop_front()
            .expect("test did not script enough failed-job lookups")
    }

    fn rerun_failed_jobs(&mut self, run: &WorkflowRun) -> Result<(), ProviderError> {
        self.rerun_failed_calls.push(run.id.to_string());
        Ok(())
    }

    fn rerun_job_by_id(
        &mut self,
        run: &WorkflowRun,
        job: &WorkflowJob,
    ) -> Result<(), ProviderError> {
        self.rerun_job_calls
            .push((run.id.to_string(), job.id.to_string()));
        Ok(())
    }
}

fn watch_from_fixture(case_id: &str, fixture: &str) -> CiWatch {
    let (_repository, declaration) = parsed_case(case_id, fixture);
    let train = &declaration.declaration.trains[0];
    let phase = &train.phases[0];
    let params = phase.params.as_object().unwrap();
    CiWatch::new(
        CiWatchConfig::new(
            phase.instance_id(),
            GitHubRepository::new("cortexkit/adopter-matrix").unwrap(),
            params["workflow"].as_str().unwrap(),
            params["selector"].as_str().unwrap(),
            SettleParameters::new(10, 100).unwrap(),
            params["rerun_budget"].as_u64().unwrap() as u32,
        )
        .unwrap(),
    )
}

fn workflow_run(watch: &CiWatch, run_id: &str) -> WorkflowRun {
    WorkflowRun {
        repository: watch.config().repository.clone(),
        id: WorkflowRunId::new(run_id).unwrap(),
        workflow: watch.config().workflow.clone(),
        selector: watch.config().selector.clone(),
        settle: watch.config().settle,
    }
}

fn failed_poll(run_id: &str, commit: &str) -> WorkflowRunPoll {
    WorkflowRunPoll::Failed(MatchingEvidence {
        reference: format!("github-actions-run:{run_id}"),
        identity: commit.to_owned(),
    })
}

fn failed_job(job_id: &str, state: WorkflowJobState) -> WorkflowJob {
    WorkflowJob {
        id: WorkflowJobId::new(job_id).unwrap(),
        name: format!("job-{job_id}"),
        state,
    }
}

#[test]
fn adopter_case_synthetic_e2e_01_reconciles_interrupted_train() {
    let (repository, declaration, plan) = planned_case(
        SYNTHETIC_E2E_01,
        SYNTHETIC_FIXTURE,
        &[("archive", "archive-v1.0.0")],
    );
    let (_state_home, journal) = journal_for(SYNTHETIC_E2E_01, &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let effect = plan.public_effects.first().unwrap();
    let effect_evidence = evidence("archive-v1.0.0");
    let effect_path = repository
        .path()
        .join("public-effects")
        .join(effect.operation.to_string());
    let mut absent_probe = FileEffectProbe::new(effect_path.clone(), effect_evidence.clone());
    let mut interrupted =
        InterruptAfterRecordedEffect::new(effect_path.clone(), effect_evidence.clone());

    assert!(reconcile_effect(
        &plan,
        &journal,
        effect,
        &mut absent_probe,
        &mut interrupted,
        &subject,
    )
    .is_err());
    assert_eq!(interrupted.calls, 1);
    assert_eq!(journal.pending_intents().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(&effect_path).unwrap(),
        format!(
            "{}\n{}",
            effect_evidence.reference, effect_evidence.identity
        )
    );

    let mut present_probe = FileEffectProbe::new(effect_path, effect_evidence.clone());
    let mut never_refired = CountingExecutor::default();
    assert_eq!(
        reconcile_effect(
            &plan,
            &journal,
            effect,
            &mut present_probe,
            &mut never_refired,
            &subject,
        )
        .unwrap(),
        EffectOutcome::Reconciled(effect_evidence)
    );
    assert_eq!(present_probe.calls, 1);
    assert_eq!(never_refired.calls, 0);
    assert!(journal.pending_intents().unwrap().is_empty());
}

#[test]
fn adopter_case_mc_saga_01_precheck_dirty_before_mutation_refuses() {
    let (repository, declaration, plan) = planned_case_with_shape(
        MC_SAGA_01,
        support::RepositoryShape::DirtyTree,
        MC_SAGA_01_FIXTURE,
        &[("crate", "v0.40.1")],
    );
    let before = fs::read_to_string(repository.path().join("README.md")).unwrap();

    let (result, records, executor_calls) =
        execute_prechecks(MC_SAGA_01, &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::PrecheckDirty);
    let message = result.unwrap_err().to_string();
    assert!(message.contains("README.md"));
    assert!(message.contains("git diff --check"));
    assert_eq!(executor_calls, 0);
    assert_eq!(
        fs::read_to_string(repository.path().join("README.md")).unwrap(),
        before
    );
}

#[test]
fn precheck_format_dirty_passes_current_live_train_mutation() {
    let fixture = r#"{
      "version": 1,
      "trains": [{
        "id": "mc-format-own-live",
        "intended_commit": "mc01-live",
        "signing_profile": "none",
        "phases": [{
          "id": "format-precheck",
          "type": "precheck-format-dirty",
          "params": {"tool": "git diff --check", "command": ["git", "diff", "--check"]}
        }]
      }]
    }"#;
    let (repository, declaration, plan) = planned_case_with_shape(
        "mc-saga-01-own-live",
        support::RepositoryShape::DirtyTree,
        fixture,
        &[],
    );
    let (result, records, executor_calls) = execute_prechecks_after(
        "mc-saga-01-own-live",
        &repository,
        &declaration,
        &plan,
        |_, journal| {
            journal
                .append_journal(JournalRecord::WorkingTreeMutation {
                    phase: "version-bump".into(),
                    paths: vec![PathBuf::from("README.md")],
                })
                .unwrap();
        },
    );

    assert!(result.is_ok());
    assert_eq!(executor_calls, 0);
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::PhaseDone { phase, .. } if phase.as_str() == "format-precheck"
    )));
    assert!(fs::read_to_string(repository.path().join("README.md"))
        .unwrap()
        .contains("dirty change"));
}

#[test]
fn adopter_case_mc_saga_02_defect_terminal_no_retry() {
    let mut watch = watch_from_fixture(MC_SAGA_02, MC_SAGA_02_FIXTURE);
    let run = workflow_run(&watch, "mc02-run");
    let mut provider = ScriptedGitHubProvider::new(
        [Ok(WorkflowRunCapture::Captured(run.clone()))],
        [Ok(failed_poll("mc02-run", "mc02"))],
        [Ok(Some(failed_job(
            "product-defect",
            WorkflowJobState::Failed,
        )))],
    );
    let mut journal = RecordingCiJournal::default();

    let conclusion = watch.step(&mut provider, &mut journal).unwrap();

    assert!(matches!(
        conclusion,
        CiWatchConclusion::Failed {
            first_failed_job: Some(ref job),
            ..
        } if job.name == "job-product-defect"
    ));
    assert_eq!(watch.reruns_used(), 0);
    assert!(provider.rerun_failed_calls.is_empty());
    assert!(matches!(
        journal.0.as_slice(),
        [
            CiWatchJournalRecord::RunCaptured { .. },
            CiWatchJournalRecord::Failed {
                rerun_budget: 0,
                ..
            }
        ]
    ));
}

#[test]
fn adopter_case_mc_saga_03_load_flake_retry_with_lock_mechanism_pending() {
    let mut watch = watch_from_fixture(MC_SAGA_03, MC_SAGA_03_FIXTURE);
    let run = workflow_run(&watch, "mc03-run");
    let mut provider = ScriptedGitHubProvider::new(
        [Ok(WorkflowRunCapture::Captured(run))],
        [Ok(failed_poll("mc03-run", "mc03"))],
        [Ok(Some(failed_job(
            "load-gate",
            WorkflowJobState::Cancelled,
        )))],
    );
    let mut journal = RecordingCiJournal::default();

    assert!(matches!(
        watch.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Undecidable(_)
    ));
    assert_eq!(watch.reruns_used(), 1);
    assert_eq!(
        provider.rerun_job_calls,
        [("mc03-run".to_owned(), "load-gate".to_owned())]
    );
    assert!(journal.0.iter().any(|record| matches!(
        record,
        CiWatchJournalRecord::RerunByJobId { instance, run_id, job_id, rerun_number: 1 }
            if instance.as_str() == "load-gate" && run_id == "mc03-run" && job_id == "load-gate"
    )));
}

#[test]
fn adopter_case_mc_saga_04_runner_vanished_resume_exactly_once() {
    let (repository, declaration, plan) = planned_case(
        MC_SAGA_04,
        MC_SAGA_04_FIXTURE,
        &[("archive", "archive-mc04")],
    );
    let (_state_home, journal) = journal_for(MC_SAGA_04, &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let effect = plan.public_effects.first().unwrap();
    let effect_evidence = evidence("archive-mc04");
    let effect_path = repository
        .path()
        .join("public-effects")
        .join(effect.operation.to_string());
    let mut absent = FileEffectProbe::new(effect_path.clone(), effect_evidence.clone());
    let mut interrupted =
        InterruptAfterRecordedEffect::new(effect_path.clone(), effect_evidence.clone());
    let _ = reconcile_effect(
        &plan,
        &journal,
        effect,
        &mut absent,
        &mut interrupted,
        &subject,
    );

    assert_eq!(
        fs::read_to_string(&effect_path).unwrap(),
        format!(
            "{}\n{}",
            effect_evidence.reference, effect_evidence.identity
        )
    );
    let mut present = FileEffectProbe::new(effect_path, effect_evidence);
    let mut never_refired = CountingExecutor::default();
    assert!(matches!(
        reconcile_effect(
            &plan,
            &journal,
            effect,
            &mut present,
            &mut never_refired,
            &subject,
        ),
        Ok(EffectOutcome::Reconciled(_))
    ));
    assert_eq!(interrupted.calls, 1);
    assert_eq!(never_refired.calls, 0);
}

#[test]
fn adopter_case_mc_saga_05_stale_residue_refuses() {
    let (repository, declaration, plan) = planned_case_with_shape(
        MC_SAGA_05,
        support::RepositoryShape::StaleWorkingTreeResidue,
        MC_SAGA_05_FIXTURE,
        &[],
    );
    let before = repository
        .residue_paths()
        .iter()
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect::<Vec<_>>();

    let (result, records, executor_calls) =
        execute_prechecks(MC_SAGA_05, &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::StaleRunResidue);
    let message = result.unwrap_err().to_string();
    assert!(message.contains("VERSION"));
    assert!(message.contains("Cargo.lock"));
    assert_eq!(executor_calls, 0);
    for (path, bytes) in before {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn precheck_stale_residue_passes_current_live_train_mutations() {
    let (repository, declaration, plan) = planned_case_with_shape(
        "mc-saga-05-own-live",
        support::RepositoryShape::StaleWorkingTreeResidue,
        MC_SAGA_05_FIXTURE,
        &[],
    );
    let (result, records, executor_calls) = execute_prechecks_after(
        "mc-saga-05-own-live",
        &repository,
        &declaration,
        &plan,
        |_, journal| {
            journal
                .append_journal(JournalRecord::WorkingTreeMutation {
                    phase: "version-bump".into(),
                    paths: vec![PathBuf::from("VERSION"), PathBuf::from("Cargo.lock")],
                })
                .unwrap();
        },
    );

    assert!(result.is_ok());
    assert_eq!(executor_calls, 0);
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::PhaseDone { phase, .. } if phase.as_str() == "stale-residue-precheck"
    )));
    assert!(repository.residue_paths().iter().all(|path| path.exists()));
}

#[test]
fn precheck_stale_residue_refuses_dead_train_mutations_by_train_id() {
    let (repository, declaration, plan) = planned_case_with_shape(
        "mc-saga-05-dead-r9",
        support::RepositoryShape::StaleWorkingTreeResidue,
        MC_SAGA_05_FIXTURE,
        &[],
    );
    let (result, records, executor_calls) = execute_prechecks_after(
        "mc-saga-05-dead-r9",
        &repository,
        &declaration,
        &plan,
        |state_home, _| {
            let predecessor = JournalStore::new(
                state_home,
                TrainJournalIdentity::new(plan.repository.clone(), plan.train.clone(), "r9-dead")
                    .unwrap(),
            )
            .unwrap();
            predecessor.pin_declaration(&declaration).unwrap();
            predecessor
                .append_journal(JournalRecord::WorkingTreeMutation {
                    phase: "version-bump".into(),
                    paths: vec![PathBuf::from("VERSION"), PathBuf::from("Cargo.lock")],
                })
                .unwrap();
            predecessor
                .append_journal(JournalRecord::Terminalized {
                    state: TrainTerminalState::Abandoned {
                        declaration_digest: declaration.digest.clone(),
                    },
                })
                .unwrap();
        },
    );

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::StaleRunResidue);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("mc-stale-residue-r9-dead"));
    assert_eq!(executor_calls, 0);
    assert!(repository.residue_paths().iter().all(|path| path.exists()));
}

#[test]
fn adopter_case_mc_saga_06_sibling_drift_refuses() {
    let repository = support::MintedRepo::mint_with_declaration(
        support::RepositoryShape::SiblingCheckoutDrift,
        MC_SAGA_06_FIXTURE,
    )
    .unwrap();
    let sibling = repository.sibling_checkout_drift().unwrap();
    let escaped_path = serde_json::to_string(&sibling.path).unwrap();
    let source = MC_SAGA_06_FIXTURE
        .replace("\"__SIBLING_PATH__\"", &escaped_path)
        .replace("__EXPECTED_REF__", &sibling.pinned_commit);
    repository.write_declaration(&source).unwrap();
    let declaration = parse(&source).unwrap();
    let plan = build_dry_run_plan(
        RepositoryId::new("adopter-mc-saga-06"),
        &declaration,
        "mc-environment-drift",
        &[],
    )
    .unwrap();
    let before = fs::read_to_string(sibling.path.join("API.md")).unwrap();

    let (result, records, executor_calls) =
        execute_prechecks(MC_SAGA_06, &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::EnvDrift);
    let message = result.unwrap_err().to_string();
    assert!(message.contains("mc-api"));
    assert!(message.contains(&sibling.pinned_commit));
    assert!(message.contains(&sibling.current_commit));
    assert_eq!(executor_calls, 0);
    assert_eq!(
        fs::read_to_string(sibling.path.join("API.md")).unwrap(),
        before
    );
}

#[test]
fn adopter_case_mc_saga_07_context_unfit_refuses_precheck() {
    let (repository, declaration, plan) = planned_case(MC_SAGA_07, MC_SAGA_07_FIXTURE, &[]);
    let before = fs::read(repository.declaration_path()).unwrap();

    let (result, records, executor_calls) =
        execute_prechecks(MC_SAGA_07, &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::ContextUnfit);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("CK_RELEASE_MC_CONTEXT_READY"));
    assert_eq!(executor_calls, 0);
    assert_eq!(fs::read(repository.declaration_path()).unwrap(), before);
}

#[test]
fn adopter_case_mc_saga_08_remote_ci_red_blocks() {
    let mut watch = watch_from_fixture(MC_SAGA_08, MC_SAGA_08_FIXTURE);
    let run = workflow_run(&watch, "mc08-run");
    let mut provider = ScriptedGitHubProvider::new(
        [Ok(WorkflowRunCapture::Captured(run))],
        [Ok(failed_poll("mc08-run", "mc08"))],
        [Ok(Some(failed_job("remote-red", WorkflowJobState::Failed)))],
    );
    let mut journal = RecordingCiJournal::default();

    assert!(matches!(
        watch.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Failed {
            first_failed_job: Some(ref job),
            ..
        } if job.name == "job-remote-red"
    ));
    assert!(provider.rerun_failed_calls.is_empty());
    assert!(matches!(
        journal.0.last(),
        Some(CiWatchJournalRecord::Failed { instance, .. }) if instance.as_str() == "remote-ci"
    ));
}

#[test]
fn adopter_case_mc_saga_09_skip_cascade_publish_incomplete_mechanism_pending() {
    let (_repository, declaration, plan) = planned_case(
        MC_SAGA_09,
        MC_SAGA_09_FIXTURE,
        &[
            ("registry-crate", "v0.40.9"),
            ("release-archive", "archive-mc09"),
        ],
    );
    assert_eq!(plan.public_effects.len(), 2);
    assert_ne!(
        plan.public_effects[0].artifact,
        plan.public_effects[1].artifact
    );

    let (_state_home, journal) = journal_for(MC_SAGA_09, &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let mut present = ScriptedProbe::new([ProbeResult::Present(evidence("v0.40.9"))]);
    let mut never_for_present = CountingExecutor::default();
    assert!(matches!(
        reconcile_effect(
            &plan,
            &journal,
            &plan.public_effects[0],
            &mut present,
            &mut never_for_present,
            &subject,
        ),
        Ok(EffectOutcome::Reconciled(_))
    ));

    let mut absent = ScriptedProbe::new([ProbeResult::Absent(evidence("archive-mc09"))]);
    let mut missing_effect = CountingExecutor::default();
    assert!(reconcile_effect(
        &plan,
        &journal,
        &plan.public_effects[1],
        &mut absent,
        &mut missing_effect,
        &subject,
    )
    .is_err());
    assert_eq!(never_for_present.calls, 0);
    assert_eq!(missing_effect.calls, 1);
}

#[test]
fn adopter_case_mc_saga_10_unpinned_tool_refuses() {
    let (repository, declaration, plan) =
        planned_case(MC_SAGA_10, MC_SAGA_10_FIXTURE, &[("tool", "tool-mc10")]);
    let before = fs::read(repository.declaration_path()).unwrap();

    let (result, records, executor_calls) =
        execute_prechecks(MC_SAGA_10, &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::ToolUnpinned);
    assert!(result.unwrap_err().to_string().contains("cargo"));
    assert_eq!(executor_calls, 0);
    assert_eq!(fs::read(repository.declaration_path()).unwrap(), before);
}

#[test]
fn precheck_tool_pinning_refuses_observed_version_mismatch() {
    let fixture = MC_SAGA_10_FIXTURE.replace(
        "\"command\": \"cargo\"",
        "\"command\": \"cargo\", \"exact_version\": \"0.0.0\"",
    );
    let (repository, declaration, plan) =
        planned_case("mc-saga-10-mismatch", &fixture, &[("tool", "tool-mc10")]);

    let (result, records, executor_calls) =
        execute_prechecks("mc-saga-10-mismatch", &repository, &declaration, &plan);

    assert_precheck_refusal(&result, &records, PrecheckRefusalCode::ToolMismatch);
    let message = result.unwrap_err().to_string();
    assert!(message.contains("expected 0.0.0"));
    assert!(message.contains("observed"));
    assert_eq!(executor_calls, 0);
}

#[test]
fn adopter_case_mc_saga_11_residue_swept_or_refused() {
    let (repository, declaration, plan) = planned_case_with_shape(
        MC_SAGA_11,
        support::RepositoryShape::RuntimeResidueFiles,
        MC_SAGA_11_FIXTURE,
        &[],
    );
    let process = repository.residue_paths()[0].clone();
    let foreign_lock = repository.residue_paths()[1].clone();
    let temporary = repository.residue_paths()[2].clone();

    let (refused, records, executor_calls) =
        execute_prechecks("mc-saga-11-refused", &repository, &declaration, &plan);
    assert_precheck_refusal(&refused, &records, PrecheckRefusalCode::ResiduePresent);
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("release-port-owner"));
    assert_eq!(executor_calls, 0);
    assert!(process.exists());
    assert!(temporary.exists());

    fs::remove_file(&foreign_lock).unwrap();
    let (swept, records, executor_calls) =
        execute_prechecks("mc-saga-11-swept", &repository, &declaration, &plan);
    assert!(swept.is_ok());
    assert_eq!(executor_calls, 0);
    assert!(!process.exists());
    assert!(!temporary.exists());
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::ResidueSwept { paths, .. }
            if paths.iter().any(|path| path.ends_with("process-1234.pid"))
                && paths.iter().any(|path| path.ends_with("session.tmp"))
    )));
}

#[test]
fn precheck_residue_sweep_passes_current_live_train_residue() {
    let (repository, declaration, plan) = planned_case_with_shape(
        "mc-saga-11-own-live",
        support::RepositoryShape::RuntimeResidueFiles,
        MC_SAGA_11_FIXTURE,
        &[],
    );
    fs::remove_file(&repository.residue_paths()[1]).unwrap();
    let (result, records, executor_calls) = execute_prechecks_after(
        "mc-saga-11-own-live",
        &repository,
        &declaration,
        &plan,
        |_, journal| {
            journal
                .append_journal(JournalRecord::WorkingTreeMutation {
                    phase: "runtime-start".into(),
                    paths: vec![
                        PathBuf::from(".cortexkit/release-residue/process-1234.pid"),
                        PathBuf::from("target/release-residue/session.tmp"),
                    ],
                })
                .unwrap();
        },
    );

    assert!(result.is_ok());
    assert_eq!(executor_calls, 0);
    assert!(repository.residue_paths()[0].exists());
    assert!(repository.residue_paths()[2].exists());
    assert!(!records
        .iter()
        .any(|record| matches!(record, JournalRecord::ResidueSwept { .. })));
}

#[test]
fn adopter_case_alf_notag_01_reconciles_embedded_build_sha() {
    let (_repository, declaration, plan) = planned_case(
        ALF_NOTAG_01,
        ALF_NOTAG_FIXTURE,
        &[("prefrontal-app", "a1b2c3d4")],
    );
    let train = &declaration.declaration.trains[0];
    let mut watch = watch_from_fixture(ALF_NOTAG_01, ALF_NOTAG_FIXTURE);
    let run = workflow_run(&watch, "alf-tests");
    let mut provider = ScriptedGitHubProvider::new(
        [Ok(WorkflowRunCapture::Captured(run))],
        [Ok(WorkflowRunPoll::Succeeded(MatchingEvidence {
            reference: "github-actions-run:alf-tests".to_owned(),
            identity: "a1b2c3d4".to_owned(),
        }))],
        [],
    );
    let mut journal = RecordingCiJournal::default();

    assert!(
        matches!(plan.release_identity, ReleaseIdentity::RunId(ref key) if key == "alf-deploy-a1b2c3d4")
    );
    assert!(train.tag.is_none());
    assert!(plan.public_effects.is_empty());
    assert!(plan.probes.iter().any(|probe| {
        probe.identity_channel == "embedded_build_sha" && probe.expected_identity == "a1b2c3d4"
    }));
    assert!(matches!(
        watch.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Succeeded(ref evidence) if evidence.identity == "a1b2c3d4"
    ));
}

#[test]
fn adopter_case_aft_ciw_01_keeps_pre_tag_watch_independent() {
    let mut pre_tag = watch_from_fixture(AFT_CIW_01, AFT_CIW_01_FIXTURE);
    let run = workflow_run(&pre_tag, "aft-tests-run");
    let mut provider = ScriptedGitHubProvider::new(
        [Ok(WorkflowRunCapture::Captured(run))],
        [Ok(failed_poll("aft-tests-run", "deadbeef"))],
        [Ok(Some(failed_job("tests-red", WorkflowJobState::Failed)))],
    );
    let mut journal = RecordingCiJournal::default();

    assert!(matches!(
        pre_tag.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Undecidable(_)
    ));
    assert_eq!(pre_tag.config().selector, "commit:deadbeef");
    assert_eq!(pre_tag.config().rerun_budget, 2);
    assert_eq!(pre_tag.reruns_used(), 1);
    assert_eq!(provider.rerun_failed_calls, ["aft-tests-run"]);
    assert!(journal.0.iter().any(|record| matches!(
        record,
        CiWatchJournalRecord::RerunFailed { instance, run_id, rerun_number: 1 }
            if instance.as_str() == "tests-before-tag" && run_id == "aft-tests-run"
    )));
}

#[test]
fn adopter_case_aft_ciw_02_keeps_post_tag_watch_independent() {
    let mut pre_tag = watch_from_fixture(AFT_CIW_01, AFT_CIW_01_FIXTURE);
    let mut post_tag = watch_from_fixture(AFT_CIW_02, AFT_CIW_02_FIXTURE);
    let pre_run = workflow_run(&pre_tag, "aft-pre-run");
    let post_run = workflow_run(&post_tag, "aft-post-run");
    let mut provider = ScriptedGitHubProvider::new(
        [
            Ok(WorkflowRunCapture::Captured(pre_run.clone())),
            Ok(WorkflowRunCapture::Captured(post_run.clone())),
        ],
        [
            Ok(failed_poll("aft-pre-run", "deadbeef")),
            Ok(failed_poll("aft-post-run", "deadbeef")),
        ],
        [
            Ok(Some(failed_job("pre-red", WorkflowJobState::Failed))),
            Ok(Some(failed_job("post-red", WorkflowJobState::Failed))),
        ],
    );
    let mut journal = RecordingCiJournal::default();

    assert!(matches!(
        pre_tag.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Undecidable(_)
    ));
    assert!(matches!(
        post_tag.step(&mut provider, &mut journal).unwrap(),
        CiWatchConclusion::Failed { .. }
    ));
    assert_eq!(pre_tag.run(), Some(&pre_run));
    assert_eq!(post_tag.run(), Some(&post_run));
    assert_eq!(pre_tag.config().selector, "commit:deadbeef");
    assert_eq!(post_tag.config().selector, "tag:v1.2.3");
    assert_eq!(pre_tag.reruns_used(), 1);
    assert_eq!(post_tag.reruns_used(), 0);
    assert_eq!(provider.rerun_failed_calls, ["aft-pre-run"]);
    assert!(journal.0.iter().any(|record| matches!(
        record,
        CiWatchJournalRecord::RunCaptured { instance, run, .. }
            if instance.as_str() == "tests-before-tag" && run.id == WorkflowRunId::new("aft-pre-run").unwrap()
    )));
    assert!(journal.0.iter().any(|record| matches!(
        record,
        CiWatchJournalRecord::RunCaptured { instance, run, .. }
            if instance.as_str() == "release-after-tag" && run.id == WorkflowRunId::new("aft-post-run").unwrap()
    )));
}

#[test]
fn undecidable_done_probe_preserves_attempt_and_never_refires() {
    let (_repository, declaration, plan) = planned_case(
        SYNTHETIC_E2E_01,
        SYNTHETIC_FIXTURE,
        &[("archive", "archive-v1.0.0")],
    );
    let (_state_home, journal) = journal_for("undecidable-probe", &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let effect = plan.public_effects.first().unwrap();
    let request = EffectRequest {
        repository: plan.repository.clone(),
        train: plan.train.clone(),
        phase: effect.phase.clone(),
        artifact: effect.artifact.clone().unwrap(),
        operation: effect.operation.clone(),
        intended_commit: plan.intended_commit.clone(),
        declaration_digest: plan.declaration_digest.clone(),
    };
    journal
        .append_intent(&request, durable_subject(&subject))
        .unwrap();
    let mut undecidable = ScriptedProbe::new([ProbeResult::Undecidable(
        cortexkit_release::UndecidableProbe {
            reason: "registry propagation is still settling".to_owned(),
            retry_after_ms: 10,
            settle_deadline_ms: 100,
        },
    )]);
    let mut never_refired = CountingExecutor::default();

    assert_eq!(
        reconcile_effect(
            &plan,
            &journal,
            effect,
            &mut undecidable,
            &mut never_refired,
            &subject,
        )
        .unwrap(),
        EffectOutcome::AwaitingProbe
    );
    assert_eq!(never_refired.calls, 0);
    assert_eq!(journal.pending_intents().unwrap().len(), 1);
}

#[test]
fn retag_or_other_material_change_requires_fresh_approval() {
    let (_repository, declaration, original_plan) = planned_case(
        SYNTHETIC_E2E_01,
        SYNTHETIC_FIXTURE,
        &[("archive", "archive-v1.0.0")],
    );
    let retagged = parse(&SYNTHETIC_FIXTURE.replace("v1.0.0", "v1.0.1")).unwrap();
    let retagged_plan = build_dry_run_plan(
        original_plan.repository.clone(),
        &retagged,
        "synthetic",
        &[FinalizedArtifact {
            artifact: ArtifactId::new("archive"),
            identity: "archive-v1.0.1".to_owned(),
            bytes: b"retagged archive".to_vec(),
        }],
    )
    .unwrap();
    let changed_bytes_plan = build_dry_run_plan(
        original_plan.repository.clone(),
        &declaration,
        "synthetic",
        &[FinalizedArtifact {
            artifact: ArtifactId::new("archive"),
            identity: "archive-v1.0.0".to_owned(),
            bytes: b"materially different archive bytes".to_vec(),
        }],
    )
    .unwrap();
    let original_subject = build_approval_subject(&original_plan).unwrap();

    for changed_plan in [&retagged_plan, &changed_bytes_plan] {
        let state_root = tempfile::tempdir().unwrap();
        let identity = TrainJournalIdentity::new(
            original_plan.repository.clone(),
            original_plan.train.clone(),
            "approval-material-change",
        )
        .unwrap();
        let approvals = ApprovalStore::new(state_root.path(), identity).unwrap();
        approvals
            .persist_confirmed(
                original_subject.clone(),
                cortexkit_release::ApprovalToken::new("original-approval"),
            )
            .unwrap();
        let changed_subject = build_approval_subject(changed_plan).unwrap();

        assert!(matches!(
            approvals.require_current(&changed_subject),
            Err(ApprovalError::SubjectMismatch)
        ));
        assert!(approvals.invalidate_if_stale(&changed_subject).unwrap());
        assert!(matches!(
            approvals.require_current(&changed_subject),
            Err(ApprovalError::NoCurrentApproval)
        ));
    }
}
