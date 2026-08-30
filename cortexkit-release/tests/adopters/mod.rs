//! Stable adopter-case acceptance matrix for the release machine.
//!
//! Every declaration is copied into a Git repository minted for the test. Public
//! effects use recording seams, so this matrix never contacts a release provider.

#[path = "../support/mod.rs"]
mod support;

use cortexkit_release::{
    approval::{build_approval_subject, ApprovalError, ApprovalStore, ApprovalSubject},
    declaration::{parse, DeclarationRefusalCode, ParsedDeclaration},
    orchestrator::{reconcile_effect, EffectOutcome, OrchestrationError, OrchestrationRefusalCode},
    phases::ci_watch::{
        CiWatch, CiWatchConclusion, CiWatchConfig, CiWatchError, CiWatchJournal,
        CiWatchJournalRecord,
    },
    plan::{build_dry_run_plan, FinalizedArtifact, ReleaseIdentity, ReleasePlan},
    provider::{
        GitHubProvider, GitHubRepository, MatchingEvidence, ProviderError, SettleParameters,
        WorkflowJob, WorkflowJobId, WorkflowJobState, WorkflowRun, WorkflowRunCapture,
        WorkflowRunId, WorkflowRunPoll, WorkflowRunRequest,
    },
    state::{JournalStore, StateError, TrainJournalIdentity},
    ApprovalSubject as DurableApprovalSubject, ArtifactDigest, ArtifactId, CompletionProbe,
    EffectRequest, IrreversibleExecutor, ProbeEvidence, ProbeResult, RepositoryId, SeamError,
};
use std::{collections::VecDeque, fs};
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
    let repository =
        support::MintedRepo::mint_with_declaration(support::RepositoryShape::Valid, fixture)
            .unwrap_or_else(|error| {
                panic!("{case_id}: could not mint hermetic repository: {error}")
            });
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
    let (repository, declaration) = parsed_case(case_id, fixture);
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
    fn execute(&mut self, _: &EffectRequest) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        Err(SeamError::new(
            "recording executor must not receive an irreversible re-fire",
        ))
    }
}

#[derive(Default)]
struct InterruptAfterRecordedEffect {
    calls: usize,
}

impl IrreversibleExecutor for InterruptAfterRecordedEffect {
    fn execute(&mut self, _: &EffectRequest) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        Err(SeamError::new(
            "interrupted after recording fake public effect before completion append",
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
    let (_repository, declaration, plan) = planned_case(
        SYNTHETIC_E2E_01,
        SYNTHETIC_FIXTURE,
        &[("archive", "archive-v1.0.0")],
    );
    let (_state_home, journal) = journal_for(SYNTHETIC_E2E_01, &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let effect = plan.public_effects.first().unwrap();
    let mut absent_probe = ScriptedProbe::new([ProbeResult::Absent(evidence("archive-v1.0.0"))]);
    let mut interrupted = InterruptAfterRecordedEffect::default();

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

    let mut present_probe = ScriptedProbe::new([ProbeResult::Present(evidence("archive-v1.0.0"))]);
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
        EffectOutcome::Reconciled(evidence("archive-v1.0.0"))
    );
    assert_eq!(present_probe.calls, 1);
    assert_eq!(never_refired.calls, 0);
    assert!(journal.pending_intents().unwrap().is_empty());
}

#[test]
fn adopter_case_mc_saga_01_precheck_dirty_before_mutation() {
    let repository = support::MintedRepo::mint_with_declaration(
        support::RepositoryShape::Valid,
        MC_SAGA_01_FIXTURE,
    )
    .unwrap();
    let source = fs::read_to_string(repository.declaration_path()).unwrap();
    let error = parse(&source).expect_err(&format!(
        "{MC_SAGA_01}: late format precheck must be refused before execution"
    ));

    assert_eq!(error.code, DeclarationRefusalCode::UnsafePhaseOrdering);
    assert!(error.message.contains("format-precheck"));
    assert!(error.message.contains("push-tag"));
    assert!(repository.path().join(".git").is_dir());
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
fn adopter_case_mc_saga_03_load_flake_retry_with_lock() {
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
    let (_repository, declaration, plan) = planned_case(
        MC_SAGA_04,
        MC_SAGA_04_FIXTURE,
        &[("archive", "archive-mc04")],
    );
    let (_state_home, journal) = journal_for(MC_SAGA_04, &declaration, &plan);
    let subject = build_approval_subject(&plan).unwrap();
    let effect = plan.public_effects.first().unwrap();
    let mut absent = ScriptedProbe::new([ProbeResult::Absent(evidence("archive-mc04"))]);
    let mut interrupted = InterruptAfterRecordedEffect::default();
    let _ = reconcile_effect(
        &plan,
        &journal,
        effect,
        &mut absent,
        &mut interrupted,
        &subject,
    );

    let mut present = ScriptedProbe::new([ProbeResult::Present(evidence("archive-mc04"))]);
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
fn adopter_case_mc_saga_05_stale_residue_reconciles() {
    let (_repository, declaration, plan) = planned_case(
        MC_SAGA_05,
        MC_SAGA_05_FIXTURE,
        &[("archive", "archive-mc05")],
    );
    let (_state_home, journal) = journal_for(MC_SAGA_05, &declaration, &plan);
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
    let mut absent = ScriptedProbe::new([ProbeResult::Absent(evidence("archive-mc05"))]);
    let mut never_refired = CountingExecutor::default();

    assert!(matches!(
        reconcile_effect(
            &plan,
            &journal,
            effect,
            &mut absent,
            &mut never_refired,
            &subject,
        ),
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::AttemptedIntentAbsent,
            ..
        })
    ));
    assert_eq!(never_refired.calls, 0);
    assert_eq!(journal.pending_intents().unwrap().len(), 1);
}

#[test]
fn adopter_case_mc_saga_06_sibling_drift_env_named() {
    let (_repository, declaration, plan) = planned_case(
        MC_SAGA_06,
        MC_SAGA_06_FIXTURE,
        &[("archive", "archive-mc06")],
    );
    let (_state_home, journal) = journal_for(MC_SAGA_06, &declaration, &plan);
    let replacement = parse(&MC_SAGA_06_FIXTURE.replace(
        "\"signing_profile\": \"none\"",
        "\"signing_profile\": \"minisign\"",
    ))
    .unwrap();
    let replacement_plan = build_dry_run_plan(
        plan.repository.clone(),
        &replacement,
        "mc-environment-drift",
        &[FinalizedArtifact {
            artifact: ArtifactId::new("archive"),
            identity: "archive-mc06".to_owned(),
            bytes: b"changed declaration must not execute".to_vec(),
        }],
    )
    .unwrap();
    let subject = build_approval_subject(&replacement_plan).unwrap();
    let effect = replacement_plan.public_effects.first().unwrap();
    let mut probe = ScriptedProbe::new([ProbeResult::Absent(evidence("archive-mc06"))]);
    let mut executor = CountingExecutor::default();

    assert!(matches!(
        reconcile_effect(
            &replacement_plan,
            &journal,
            effect,
            &mut probe,
            &mut executor,
            &subject,
        ),
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::DeclarationDigestMismatch,
            ..
        })
    ));
    assert_eq!(probe.calls, 0);
    assert_eq!(executor.calls, 0);
}

#[test]
fn adopter_case_mc_saga_07_context_unfit_refuses_precheck() {
    let repository = support::MintedRepo::mint_with_declaration(
        support::RepositoryShape::Valid,
        MC_SAGA_07_FIXTURE,
    )
    .unwrap();
    let source = fs::read_to_string(repository.declaration_path()).unwrap();
    let error = parse(&source).expect_err(&format!(
        "{MC_SAGA_07}: incomplete context gate parameters must fail closed"
    ));

    assert_eq!(error.code, DeclarationRefusalCode::InvalidPhaseParameters);
    assert!(error.message.contains("context-gate"));
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
fn adopter_case_mc_saga_09_skip_cascade_publish_incomplete() {
    let (_repository, declaration) = parsed_case(MC_SAGA_09, MC_SAGA_09_FIXTURE);
    let train = &declaration.declaration.trains[0];

    assert!(matches!(
        build_dry_run_plan(
            RepositoryId::new("adopter-mc-saga-09"),
            &declaration,
            &train.id,
            &[]
        ),
        Err(cortexkit_release::plan::PlanError::MissingArtifact { artifact, .. })
            if artifact == "release-archive"
    ));
}

#[test]
fn adopter_case_mc_saga_10_unpinned_tool_refuses() {
    let repository = support::MintedRepo::mint_with_declaration(
        support::RepositoryShape::Valid,
        MC_SAGA_10_FIXTURE,
    )
    .unwrap();
    let source = fs::read_to_string(repository.declaration_path()).unwrap();
    let error = parse(&source).expect_err(&format!(
        "{MC_SAGA_10}: unsupported identity channel must not plan"
    ));

    assert_eq!(
        error.code,
        DeclarationRefusalCode::InvalidArtifactIdentityChannel
    );
    assert!(error.message.contains("outside_declared_pins"));
}

#[test]
fn adopter_case_mc_saga_11_residue_swept_or_refused() {
    let (_repository, declaration) = parsed_case(MC_SAGA_11, MC_SAGA_11_FIXTURE);
    let root = tempfile::tempdir().unwrap();
    let identity = TrainJournalIdentity::new(
        RepositoryId::new("adopter-mc-saga-11"),
        declaration.declaration.trains[0].train_id(),
        "runtime-residue",
    )
    .unwrap();
    let journal = JournalStore::new(root.path(), identity).unwrap();
    fs::write(journal.journal_path(), b"{\"version\":1,\"record\":").unwrap();

    assert!(matches!(
        journal.read_journal(),
        Err(StateError::TornTail { .. })
    ));
    assert!(journal.recover_torn_journal_tail().unwrap().is_empty());
    assert!(fs::read(journal.journal_path()).unwrap().is_empty());

    journal.pin_declaration(&declaration).unwrap();
    let mut corrupted = fs::read(journal.journal_path()).unwrap();
    let offset = corrupted
        .windows(b"mc-residue-sweep".len())
        .position(|window| window == b"mc-residue-sweep")
        .unwrap();
    corrupted[offset] = b'x';
    fs::write(journal.journal_path(), &corrupted).unwrap();

    assert!(matches!(
        journal.recover_torn_journal_tail(),
        Err(StateError::CorruptRecord { .. })
    ));
    assert_eq!(fs::read(journal.journal_path()).unwrap(), corrupted);
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
