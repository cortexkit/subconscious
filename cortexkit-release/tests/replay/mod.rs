use crate::{
    approval::{build_approval_subject, ApprovalStore},
    declaration::parse,
    executor::AdmittedEffect,
    lease::LeaseStore,
    orchestrator::{
        reconcile_effect_unfenced_for_tests, EffectOutcome, FirstPublicTriggerGate,
        OrchestrationError, OrchestrationRefusalCode, Orchestrator, PhaseExecutionError,
        PhaseRegistry, PhaseRunner,
    },
    plan::{build_dry_run_plan, FinalizedArtifact, PlannedPhase, ReleasePlan},
    state::{JournalRecord, JournalStore, TrainJournalIdentity},
    ApprovalToken, ArtifactId, CompletionProbe, EffectRequest, IrreversibleExecutor, ProbeEvidence,
    ProbeResult, SeamError,
};
use std::{cell::RefCell, collections::VecDeque, fs, rc::Rc};
use tempfile::TempDir;

const DECLARATION: &str = r#"
{
  "version": 1,
  "trains": [{
    "id": "release",
    "intended_commit": "commit-a",
    "tag": "v1.2.3",
    "signing_profile": "none",
    "operator_gates": ["first_public_trigger"],
    "artifacts": [{"id": "archive", "kind": "archive", "identity_channel": "asset_sha256"}],
    "phases": [
      {"id": "preflight", "type": "preflight"},
      {"id": "tag", "type": "tag"},
      {"id": "publish", "type": "publish"},
      {"id": "stage", "type": "stage"}
    ]
  }]
}
"#;

fn plan() -> ReleasePlan {
    build_dry_run_plan(
        "example-repository".into(),
        &parse(DECLARATION).unwrap(),
        "release",
        &[FinalizedArtifact {
            artifact: ArtifactId::new("archive"),
            identity: "archive-v1.2.3".to_owned(),
            bytes: b"final archive bytes".to_vec(),
        }],
    )
    .unwrap()
}

fn state(plan: &ReleasePlan) -> (TempDir, JournalStore, ApprovalStore) {
    let root = tempfile::tempdir().unwrap();
    let identity =
        TrainJournalIdentity::new(plan.repository.clone(), plan.train.clone(), "run-1").unwrap();
    let journal = JournalStore::new(root.path(), identity.clone()).unwrap();
    journal
        .pin_declaration(&parse(DECLARATION).unwrap())
        .unwrap();
    let approvals = ApprovalStore::new(root.path(), identity).unwrap();
    (root, journal, approvals)
}

fn effect(plan: &ReleasePlan) -> crate::plan::PublicEffect {
    plan.public_effects
        .iter()
        .find(|effect| effect.artifact.is_some())
        .unwrap()
        .clone()
}

fn request(plan: &ReleasePlan, effect: &crate::plan::PublicEffect) -> EffectRequest {
    EffectRequest {
        repository: plan.repository.clone(),
        train: plan.train.clone(),
        phase: effect.phase.clone(),
        artifact: effect.artifact.clone().unwrap(),
        operation: effect.operation.clone(),
        intended_commit: plan.intended_commit.clone(),
        declaration_digest: plan.declaration_digest.clone(),
    }
}

fn durable_subject(subject: &crate::approval::ApprovalSubject) -> crate::ApprovalSubject {
    crate::ApprovalSubject {
        repository: subject.repository.clone(),
        train: subject.train.clone(),
        intended_commit: subject.intended_commit.clone(),
        declaration_digest: subject.declaration_digest.clone(),
        artifact_digests: subject
            .artifacts
            .iter()
            .map(|artifact| crate::ArtifactDigest {
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

fn evidence() -> ProbeEvidence {
    ProbeEvidence {
        reference: "provider/archive-v1.2.3".to_owned(),
        identity: "archive-v1.2.3".to_owned(),
    }
}

fn undecidable() -> ProbeResult {
    ProbeResult::Undecidable(crate::UndecidableProbe {
        reason: "registry propagation is still settling".to_owned(),
        retry_after_ms: 1,
        settle_deadline_ms: 2,
    })
}

struct ScriptedProbe(VecDeque<Result<ProbeResult, SeamError>>);

impl ScriptedProbe {
    fn new(outcomes: impl IntoIterator<Item = Result<ProbeResult, SeamError>>) -> Self {
        Self(outcomes.into_iter().collect())
    }
}

impl CompletionProbe for ScriptedProbe {
    fn probe(&mut self, _: &EffectRequest) -> Result<ProbeResult, SeamError> {
        self.0.pop_front().expect("test must script every probe")
    }
}

#[derive(Default)]
struct ScriptedExecutor {
    calls: usize,
    outcomes: VecDeque<Result<ProbeEvidence, SeamError>>,
}

impl ScriptedExecutor {
    fn new(outcomes: impl IntoIterator<Item = Result<ProbeEvidence, SeamError>>) -> Self {
        Self {
            calls: 0,
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl IrreversibleExecutor for ScriptedExecutor {
    fn execute(&mut self, _: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        self.outcomes
            .pop_front()
            .expect("test must script every executor call")
    }
}

/// Simulates a process stopping only after the fake provider recorded its public effect.
#[derive(Default)]
struct InterruptAfterEffect {
    effects: usize,
}

impl IrreversibleExecutor for InterruptAfterEffect {
    fn execute(&mut self, _: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.effects += 1;
        Err(SeamError::new(
            "interrupted after fake public effect before completion append",
        ))
    }
}

#[test]
fn replay_matrix_covers_attempted_and_never_attempted_for_every_probe_result() {
    for (attempted, probe_result) in [
        (false, ProbeResult::Present(evidence())),
        (false, ProbeResult::Absent(evidence())),
        (false, undecidable()),
        (true, ProbeResult::Present(evidence())),
        (true, ProbeResult::Absent(evidence())),
        (true, undecidable()),
    ] {
        let plan = plan();
        let (_root, journal, _) = state(&plan);
        let effect = effect(&plan);
        let subject = build_approval_subject(&plan).unwrap();
        if attempted {
            journal
                .append_intent(&request(&plan, &effect), durable_subject(&subject))
                .unwrap();
        }
        let mut probe = ScriptedProbe::new([Ok(probe_result.clone())]);
        let mut executor = ScriptedExecutor::new([Ok(evidence())]);
        let result = reconcile_effect_unfenced_for_tests(
            &plan,
            &journal,
            &effect,
            &mut probe,
            &mut executor,
            &subject,
        );

        match (attempted, probe_result) {
            (false, ProbeResult::Present(_)) => {
                assert_eq!(result.unwrap(), EffectOutcome::Reconciled(evidence()));
                assert_eq!(executor.calls, 0);
                assert!(journal.read_intents().unwrap().is_empty());
            }
            (false, ProbeResult::Absent(_)) => {
                assert_eq!(result.unwrap(), EffectOutcome::Executed(evidence()));
                assert_eq!(executor.calls, 1);
                assert_eq!(journal.read_journal().unwrap().len(), 2);
            }
            (false, ProbeResult::Undecidable(_)) => {
                assert_eq!(result.unwrap(), EffectOutcome::AwaitingProbe);
                assert_eq!(executor.calls, 0);
                assert!(journal.read_intents().unwrap().is_empty());
            }
            (true, ProbeResult::Present(_)) => {
                assert_eq!(result.unwrap(), EffectOutcome::Reconciled(evidence()));
                assert_eq!(executor.calls, 0);
                assert_eq!(journal.pending_intents().unwrap().len(), 0);
            }
            (true, ProbeResult::Absent(_)) => {
                assert!(matches!(
                    result,
                    Err(OrchestrationError::Refusal {
                        code: OrchestrationRefusalCode::AttemptedIntentAbsent,
                        ..
                    })
                ));
                assert_eq!(executor.calls, 0);
                assert_eq!(journal.pending_intents().unwrap().len(), 1);
            }
            (true, ProbeResult::Undecidable(_)) => {
                assert_eq!(result.unwrap(), EffectOutcome::AwaitingProbe);
                assert_eq!(executor.calls, 0);
                assert_eq!(journal.pending_intents().unwrap().len(), 1);
            }
        }
    }
}

#[test]
fn interruption_after_public_effect_resumes_with_probe_without_duplicate_execution() {
    let plan = plan();
    let (_root, journal, _) = state(&plan);
    let effect = effect(&plan);
    let subject = build_approval_subject(&plan).unwrap();
    let mut first_probe = ScriptedProbe::new([Ok(ProbeResult::Absent(evidence()))]);
    let mut interrupted_executor = InterruptAfterEffect::default();

    assert!(reconcile_effect_unfenced_for_tests(
        &plan,
        &journal,
        &effect,
        &mut first_probe,
        &mut interrupted_executor,
        &subject,
    )
    .is_err());
    assert_eq!(interrupted_executor.effects, 1);
    assert_eq!(journal.pending_intents().unwrap().len(), 1);

    let mut resume_probe = ScriptedProbe::new([Ok(ProbeResult::Present(evidence()))]);
    let mut never_called = ScriptedExecutor::default();
    assert_eq!(
        reconcile_effect_unfenced_for_tests(
            &plan,
            &journal,
            &effect,
            &mut resume_probe,
            &mut never_called,
            &subject,
        )
        .unwrap(),
        EffectOutcome::Reconciled(evidence())
    );
    assert_eq!(never_called.calls, 0);
    assert_eq!(journal.pending_intents().unwrap().len(), 0);
}

#[test]
fn two_interruptions_do_not_append_duplicate_completion() {
    let plan = plan();
    let (_root, journal, _) = state(&plan);
    let effect = effect(&plan);
    let subject = build_approval_subject(&plan).unwrap();
    let mut first_probe = ScriptedProbe::new([Ok(ProbeResult::Absent(evidence()))]);
    let mut interrupted = InterruptAfterEffect::default();
    assert!(reconcile_effect_unfenced_for_tests(
        &plan,
        &journal,
        &effect,
        &mut first_probe,
        &mut interrupted,
        &subject,
    )
    .is_err());

    for _ in 0..2 {
        let mut present_probe = ScriptedProbe::new([Ok(ProbeResult::Present(evidence()))]);
        let mut never_called = ScriptedExecutor::default();
        assert_eq!(
            reconcile_effect_unfenced_for_tests(
                &plan,
                &journal,
                &effect,
                &mut present_probe,
                &mut never_called,
                &subject,
            )
            .unwrap(),
            EffectOutcome::Reconciled(evidence())
        );
        assert_eq!(never_called.calls, 0);
    }

    let completions = journal
        .read_journal()
        .unwrap()
        .into_iter()
        .filter(|record| matches!(record, JournalRecord::Completion { .. }))
        .count();
    assert_eq!(completions, 1);
}

#[test]
fn torn_tail_is_recovered_automatically_before_reconcile() {
    let plan = plan();
    let (_root, journal, _) = state(&plan);
    fs::write(journal.intent_path(), b"{\"version\":1,\"record\":").unwrap();
    let effect = effect(&plan);
    let subject = build_approval_subject(&plan).unwrap();
    let mut probe = ScriptedProbe::new([Ok(ProbeResult::Absent(evidence()))]);
    let mut executor = ScriptedExecutor::new([Ok(evidence())]);

    assert_eq!(
        reconcile_effect_unfenced_for_tests(
            &plan,
            &journal,
            &effect,
            &mut probe,
            &mut executor,
            &subject,
        )
        .unwrap(),
        EffectOutcome::Executed(evidence())
    );
    assert_eq!(executor.calls, 1);
    assert_eq!(journal.read_intents().unwrap().len(), 1);
}

#[test]
fn done_probe_overrides_completion_history_but_contradictions_fail_closed() {
    let plan = plan();
    let (_root, journal, _) = state(&plan);
    let effect = effect(&plan);
    let subject = build_approval_subject(&plan).unwrap();
    let intent = journal
        .append_intent(&request(&plan, &effect), durable_subject(&subject))
        .unwrap();
    journal.append_completion(&intent, evidence()).unwrap();

    let mut absent_probe = ScriptedProbe::new([Ok(ProbeResult::Absent(evidence()))]);
    let mut executor = ScriptedExecutor::default();
    assert!(matches!(
        reconcile_effect_unfenced_for_tests(
            &plan,
            &journal,
            &effect,
            &mut absent_probe,
            &mut executor,
            &subject,
        ),
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::ContradictoryEvidence,
            ..
        })
    ));
    assert_eq!(executor.calls, 0);

    let mut present_probe = ScriptedProbe::new([Ok(ProbeResult::Present(evidence()))]);
    assert_eq!(
        reconcile_effect_unfenced_for_tests(
            &plan,
            &journal,
            &effect,
            &mut present_probe,
            &mut executor,
            &subject,
        )
        .unwrap(),
        EffectOutcome::Reconciled(evidence())
    );
    assert_eq!(executor.calls, 0);
}

#[derive(Clone)]
struct Trace(Rc<RefCell<Vec<String>>>);

impl Trace {
    fn push(&self, value: impl Into<String>) {
        self.0.borrow_mut().push(value.into());
    }
}

struct RecordingRunner(Trace);

impl PhaseRunner for RecordingRunner {
    fn run(&mut self, phase: &PlannedPhase) -> Result<Vec<ProbeEvidence>, PhaseExecutionError> {
        self.0.push(phase.instance.to_string());
        Ok(Vec::new())
    }
}

struct RecordingGate(Trace);

impl FirstPublicTriggerGate for RecordingGate {
    fn confirm(
        &mut self,
        _: &crate::approval::ApprovalSubject,
    ) -> Result<ApprovalToken, SeamError> {
        self.0.push("gate");
        Ok(ApprovalToken::new("approved"))
    }
}

struct TracingExecutor {
    trace: Trace,
}

#[derive(Default)]
struct TypedWitnessExecutor {
    calls: usize,
    publication_calls: usize,
}

impl IrreversibleExecutor for TypedWitnessExecutor {
    fn execute(&mut self, admitted: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        admitted
            .validate_approval_binding()
            .map_err(|error| SeamError::new(error.to_string()))?;
        self.calls += 1;
        if let Some(publication) = admitted.publication() {
            self.publication_calls += 1;
            assert_eq!(publication.artifact.bytes(), b"final archive bytes");
            Ok(evidence())
        } else {
            Ok(ProbeEvidence {
                reference: "tag/v1.2.3".to_owned(),
                identity: "commit-a".to_owned(),
            })
        }
    }
}

struct ApprovalTamperingExecutor {
    approvals: ApprovalStore,
    stale_subject: crate::approval::ApprovalSubject,
    calls: usize,
}

impl IrreversibleExecutor for ApprovalTamperingExecutor {
    fn execute(&mut self, admitted: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        self.approvals
            .persist_confirmed(
                self.stale_subject.clone(),
                ApprovalToken::new("stale-after-first-effect"),
            )
            .map_err(|error| SeamError::new(error.to_string()))?;
        if admitted.effect().artifact.as_str() == "tag" {
            Ok(ProbeEvidence {
                reference: "tag/v1.2.3".to_owned(),
                identity: "commit-a".to_owned(),
            })
        } else {
            Ok(evidence())
        }
    }
}

impl IrreversibleExecutor for TracingExecutor {
    fn execute(&mut self, admitted: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.trace.push("execute");
        if admitted.effect().artifact.as_str() == "tag" {
            Ok(ProbeEvidence {
                reference: "tag/v1.2.3".to_owned(),
                identity: "commit-a".to_owned(),
            })
        } else {
            Ok(evidence())
        }
    }
}

#[test]
fn exactly_once_api_boundary_runs_full_ladder_with_typed_admitted_effects() {
    let plan = plan();
    let (root, journal, approvals) = state(&plan);
    let leases = LeaseStore::new(root.path()).unwrap();
    let trace = Trace(Rc::new(RefCell::new(Vec::new())));
    let mut runner = RecordingRunner(trace.clone());
    let mut gate = RecordingGate(trace.clone());
    let mut probe = ScriptedProbe::new([
        Ok(ProbeResult::Absent(ProbeEvidence {
            identity: "commit-a".to_owned(),
            reference: "tag/v1.2.3".to_owned(),
        })),
        Ok(ProbeResult::Absent(evidence())),
    ]);
    let mut executor = TracingExecutor {
        trace: trace.clone(),
    };

    Orchestrator::new(PhaseRegistry)
        .execute(
            &plan,
            &leases,
            &journal,
            &approvals,
            &mut runner,
            &mut gate,
            &mut probe,
            &mut executor,
        )
        .unwrap();

    assert_eq!(
        trace.0.borrow().as_slice(),
        ["preflight", "gate", "execute", "execute", "stage"]
    );
}

#[test]
fn approval_digest_binding_rejects_substituted_finalized_bytes_before_executor() {
    let mut plan = plan();
    plan.finalized_artifacts[0].bytes = b"substituted after approval planning".to_vec();
    let (root, journal, approvals) = state(&plan);
    let leases = LeaseStore::new(root.path()).unwrap();
    let trace = Trace(Rc::new(RefCell::new(Vec::new())));
    let mut runner = RecordingRunner(trace.clone());
    let mut gate = RecordingGate(trace);
    let mut probe = ScriptedProbe::new([
        Ok(ProbeResult::Absent(ProbeEvidence {
            identity: "commit-a".to_owned(),
            reference: "tag/v1.2.3".to_owned(),
        })),
        Ok(ProbeResult::Absent(evidence())),
    ]);
    let mut executor = TypedWitnessExecutor::default();

    assert!(matches!(
        Orchestrator::default().execute(
            &plan,
            &leases,
            &journal,
            &approvals,
            &mut runner,
            &mut gate,
            &mut probe,
            &mut executor,
        ),
        Err(OrchestrationError::Executor(_))
    ));
    assert_eq!(executor.calls, 1);
    assert_eq!(executor.publication_calls, 0);
}

#[test]
fn current_durable_approval_is_reloaded_before_every_effect() {
    let mut plan = plan();
    plan.phases.retain(|phase| phase.phase_type != "tag");
    plan.public_effects
        .retain(|effect| effect.artifact.is_some());
    let mut second_effect = plan.public_effects[0].clone();
    second_effect.operation = "publish:archive-again".into();
    plan.public_effects.push(second_effect);
    plan.first_public_trigger = plan.public_effects.first().cloned();
    let (root, journal, approvals) = state(&plan);
    let leases = LeaseStore::new(root.path()).unwrap();
    let trace = Trace(Rc::new(RefCell::new(Vec::new())));
    let mut runner = RecordingRunner(trace.clone());
    let mut gate = RecordingGate(trace);
    let mut probe = ScriptedProbe::new([
        Ok(ProbeResult::Absent(evidence())),
        Ok(ProbeResult::Absent(evidence())),
    ]);
    let mut stale_subject = build_approval_subject(&plan).unwrap();
    stale_subject.artifacts[0].digest = "stale-digest".to_owned();
    let mut executor = ApprovalTamperingExecutor {
        approvals: approvals.clone(),
        stale_subject,
        calls: 0,
    };

    assert!(matches!(
        Orchestrator::default().execute(
            &plan,
            &leases,
            &journal,
            &approvals,
            &mut runner,
            &mut gate,
            &mut probe,
            &mut executor,
        ),
        Err(OrchestrationError::Approval(
            crate::approval::ApprovalError::SubjectMismatch
        ))
    ));
    assert_eq!(executor.calls, 1);
}

#[test]
fn irreversible_tree_mutating_phase_requires_repository_lease() {
    let plan = plan();
    let (root, journal, approvals) = state(&plan);
    let leases = LeaseStore::new(root.path()).unwrap();
    let held_repository_lease = leases.acquire_repository(plan.repository.clone()).unwrap();
    let trace = Trace(Rc::new(RefCell::new(Vec::new())));
    let mut runner = RecordingRunner(trace.clone());
    let mut gate = RecordingGate(trace.clone());
    let mut probe = ScriptedProbe::new([]);
    let mut executor = TypedWitnessExecutor::default();

    assert!(matches!(
        Orchestrator::default().execute(
            &plan,
            &leases,
            &journal,
            &approvals,
            &mut runner,
            &mut gate,
            &mut probe,
            &mut executor,
        ),
        Err(OrchestrationError::Lease(_))
    ));
    assert_eq!(trace.0.borrow().as_slice(), ["preflight"]);
    assert_eq!(executor.calls, 0);
    held_repository_lease.release().unwrap();
}

struct RefusingRunner;

impl PhaseRunner for RefusingRunner {
    fn run(&mut self, phase: &PlannedPhase) -> Result<Vec<ProbeEvidence>, PhaseExecutionError> {
        Err(SeamError::new(format!("{} refused its precondition", phase.instance)).into())
    }
}

#[test]
fn orchestrator_journals_phase_entry_done_and_refusal_records() {
    let plan = plan();
    let (root, journal, approvals) = state(&plan);
    let leases = LeaseStore::new(root.path()).unwrap();
    let trace = Trace(Rc::new(RefCell::new(Vec::new())));
    let mut runner = RecordingRunner(trace.clone());
    let mut gate = RecordingGate(trace);
    let mut probe = ScriptedProbe::new([
        Ok(ProbeResult::Absent(ProbeEvidence {
            identity: "commit-a".to_owned(),
            reference: "tag/v1.2.3".to_owned(),
        })),
        Ok(ProbeResult::Absent(evidence())),
    ]);
    let mut executor = TypedWitnessExecutor::default();
    Orchestrator::default()
        .execute(
            &plan,
            &leases,
            &journal,
            &approvals,
            &mut runner,
            &mut gate,
            &mut probe,
            &mut executor,
        )
        .unwrap();

    let records = journal.read_journal().unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, JournalRecord::PhaseEntered { .. }))
            .count(),
        4
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, JournalRecord::PhaseDone { .. }))
            .count(),
        4
    );
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::PhaseDone { phase, evidence: phase_evidence }
            if phase.as_str() == "publish" && phase_evidence == &vec![evidence()]
    )));

    let (refusal_root, refusal_journal, refusal_approvals) = state(&plan);
    let refusal_leases = LeaseStore::new(refusal_root.path()).unwrap();
    let mut refusing_runner = RefusingRunner;
    let mut unused_gate = RecordingGate(Trace(Rc::new(RefCell::new(Vec::new()))));
    let mut unused_probe = ScriptedProbe::new([]);
    let mut unused_executor = TypedWitnessExecutor::default();
    assert!(Orchestrator::default()
        .execute(
            &plan,
            &refusal_leases,
            &refusal_journal,
            &refusal_approvals,
            &mut refusing_runner,
            &mut unused_gate,
            &mut unused_probe,
            &mut unused_executor,
        )
        .is_err());
    assert!(refusal_journal
        .read_journal()
        .unwrap()
        .iter()
        .any(|record| {
            matches!(
                record,
                JournalRecord::Refused { phase, reason }
                    if phase.as_str() == "preflight" && reason.contains("refused its precondition")
            )
        }));
}

#[test]
fn declaration_and_registry_keep_post_tag_ci_watch_inexpressible() {
    let late_ci = DECLARATION.replace(
        "{\"id\": \"publish\", \"type\": \"publish\"}",
        "{\"id\": \"post-tag-ci\", \"type\": \"ci_watch\", \"params\": {\"workflow\": \"release.yml\", \"selector\": \"tag\", \"rerun_budget\": 0}}",
    );
    assert!(parse(&late_ci).is_err());

    let mut late_ci_plan = plan();
    late_ci_plan.phases = vec![
        late_ci_plan
            .phases
            .iter()
            .find(|phase| phase.phase_type == "tag")
            .unwrap()
            .clone(),
        PlannedPhase {
            instance: "post-tag-ci".into(),
            phase_type: "ci_watch".to_owned(),
            params: serde_json::json!({"workflow": "release.yml", "selector": "tag", "rerun_budget": 0}),
            tree_mutating: false,
        },
    ];
    assert!(matches!(
        PhaseRegistry.validate_plan(&late_ci_plan),
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::UnsafeOrdering,
            ..
        })
    ));
}

#[test]
fn registry_refuses_unknown_place_and_late_refusal_capable_phases() {
    let plan = plan();
    let registry = PhaseRegistry;
    assert!(registry.classify("place").is_none());

    let mut unsafe_plan = plan.clone();
    unsafe_plan.phases = vec![
        unsafe_plan
            .phases
            .iter()
            .find(|phase| phase.phase_type == "tag")
            .unwrap()
            .clone(),
        unsafe_plan
            .phases
            .iter()
            .find(|phase| phase.phase_type == "preflight")
            .unwrap()
            .clone(),
    ];
    assert!(matches!(
        registry.validate_plan(&unsafe_plan),
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::UnsafeOrdering,
            ..
        })
    ));
}
