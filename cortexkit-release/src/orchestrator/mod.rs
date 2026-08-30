//! Ordered phase execution and durable irreversible-effect reconciliation.
//!
//! The registry is intentionally machine-owned: declarations select an instance
//! of a supported phase but cannot supply implementation code.  Reconciliation
//! probes before every public effect, including effects already marked complete,
//! because provider evidence is authoritative over local journal history.

use crate::{
    approval::{build_approval_subject, ApprovalStore, ApprovalSubject},
    executor::{AdmittedEffect, ExecutorError},
    lease::LeaseStore,
    plan::{PlannedPhase, PublicEffect, ReleasePlan},
    state::{IntentRecord, JournalRecord, JournalStore, PendingIntent, StateError},
    ApprovalToken, ArtifactId, CompletionProbe, EffectRequest, IrreversibleExecutor, OperationId,
    PhaseInstanceId, ProbeEvidence, ProbeResult, SeamError,
};
use std::collections::HashSet;
use thiserror::Error;

/// The only execution classes accepted by the machine-owned phase registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseClass {
    /// A phase that may refuse and must have its first execution before publication.
    RefusalCapable,
    /// A phase that performs one or more irreversible public effects.
    Irreversible,
    /// A phase allowed after publication because it only performs admitted work or bookkeeping.
    PostBoundary,
}

/// A closed registry that classifies all declaration-supported phase types.
#[derive(Clone, Debug, Default)]
pub struct PhaseRegistry;

impl PhaseRegistry {
    /// Returns the execution class for one supported phase type.
    pub fn classify(&self, phase_type: &str) -> Option<PhaseClass> {
        match phase_type {
            "preflight"
            | "gates_local"
            | "ci_watch"
            | "build"
            | "stamp"
            | "verify_readback"
            | crate::phases::precheck::FORMAT_DIRTY
            | crate::phases::precheck::STALE_RESIDUE
            | crate::phases::precheck::SIBLING_DRIFT
            | crate::phases::precheck::CONTEXT_FITNESS
            | crate::phases::precheck::TOOL_PINNING
            | crate::phases::precheck::RESIDUE_SWEEP => Some(PhaseClass::RefusalCapable),
            "tag" | "publish" | "assets" => Some(PhaseClass::Irreversible),
            "stage" | "notify" => Some(PhaseClass::PostBoundary),
            _ => None,
        }
    }

    /// Rejects an unregistered phase and unsafe first-run ordering before execution begins.
    pub fn validate_plan(&self, plan: &ReleasePlan) -> Result<(), OrchestrationError> {
        let mut irreversible_phase: Option<&PhaseInstanceId> = None;
        for phase in &plan.phases {
            let class = self.classify(&phase.phase_type).ok_or_else(|| {
                OrchestrationError::refusal(
                    OrchestrationRefusalCode::UnknownPhase,
                    Some(phase.instance.clone()),
                    format!("phase type `{}` is not registered", phase.phase_type),
                )
            })?;
            match (irreversible_phase.as_ref(), class) {
                (Some(earlier), PhaseClass::RefusalCapable) => {
                    return Err(OrchestrationError::refusal(
                        OrchestrationRefusalCode::UnsafeOrdering,
                        Some(phase.instance.clone()),
                        format!(
                            "refusal-capable phase `{}` first executes after irreversible phase `{earlier}`",
                            phase.instance
                        ),
                    ));
                }
                (None, PhaseClass::Irreversible) => irreversible_phase = Some(&phase.instance),
                _ => {}
            }
        }
        Ok(())
    }
}

/// Stable refusal codes emitted by declared precheck detectors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrecheckRefusalCode {
    PrecheckDirty,
    StaleRunResidue,
    EnvDrift,
    ContextUnfit,
    ToolUnpinned,
    ToolMismatch,
    ResiduePresent,
}

impl std::fmt::Display for PrecheckRefusalCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::PrecheckDirty => "PRECHECK_DIRTY",
            Self::StaleRunResidue => "STALE_RUN_RESIDUE",
            Self::EnvDrift => "ENV_DRIFT",
            Self::ContextUnfit => "CONTEXT_UNFIT",
            Self::ToolUnpinned => "TOOL_UNPINNED",
            Self::ToolMismatch => "TOOL_MISMATCH",
            Self::ResiduePresent => "RESIDUE_PRESENT",
        };
        formatter.write_str(code)
    }
}

/// A typed local phase result kept distinct from an infrastructure seam failure.
#[derive(Debug)]
pub enum PhaseExecutionError {
    Refusal {
        code: PrecheckRefusalCode,
        phase: PhaseInstanceId,
        message: String,
    },
    Seam(SeamError),
}

impl From<SeamError> for PhaseExecutionError {
    fn from(error: SeamError) -> Self {
        Self::Seam(error)
    }
}

/// Executes a registered non-public phase instance in declaration order.
pub trait PhaseRunner {
    fn run(&mut self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError>;
}

/// Requests the one operator confirmation that admits the public-effect list.
pub trait FirstPublicTriggerGate {
    fn confirm(&mut self, subject: &ApprovalSubject) -> Result<ApprovalToken, SeamError>;
}

/// Durable result for one public effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectOutcome {
    /// A first attempt was admitted, journaled, and completed.
    Executed(ProbeEvidence),
    /// A probe established that an existing effect is complete without invoking the executor.
    Reconciled(ProbeEvidence),
    /// The provider cannot yet decide, so resume must probe again before any call.
    AwaitingProbe,
}

/// Typed refusals and failures returned by the orchestrator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrchestrationRefusalCode {
    UnknownPhase,
    UnsafeOrdering,
    AttemptedIntentAbsent,
    ContradictoryEvidence,
    MissingPublicEffect,
    DeclarationDigestMismatch,
}

/// An execution failure that does not erase the durable intent needed for replay.
#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("{code:?}: {message}")]
    Refusal {
        code: OrchestrationRefusalCode,
        phase: Option<PhaseInstanceId>,
        message: String,
    },
    #[error("lease failure: {0}")]
    Lease(#[from] crate::lease::LeaseError),
    #[error("journal failure: {0}")]
    State(#[from] crate::state::StateError),
    #[error("approval failure: {0}")]
    Approval(#[from] crate::approval::ApprovalError),
    #[error("publication admission failed: {0}")]
    Executor(#[from] ExecutorError),
    #[error("{code}: {message}")]
    PrecheckRefusal {
        code: PrecheckRefusalCode,
        phase: PhaseInstanceId,
        message: String,
    },
    #[error("phase or provider seam failed: {0}")]
    Seam(#[from] SeamError),
}

impl OrchestrationError {
    fn refusal(
        code: OrchestrationRefusalCode,
        phase: Option<PhaseInstanceId>,
        message: impl Into<String>,
    ) -> Self {
        Self::Refusal {
            code,
            phase,
            message: message.into(),
        }
    }
}

/// Executes a validated plan through the closed registry.
#[derive(Clone, Debug, Default)]
pub struct Orchestrator {
    registry: PhaseRegistry,
}

impl Orchestrator {
    pub fn new(registry: PhaseRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &PhaseRegistry {
        &self.registry
    }

    /// Runs phase instances in declaration order while holding the train lease.
    ///
    /// Tree-mutating phases additionally hold the repository lease. The operator
    /// gate is requested and persisted before the first irreversible effect; this
    /// API has no placement operation or placement-confirmation path.
    #[allow(clippy::too_many_arguments)]
    pub fn execute<R, G, P, E>(
        &self,
        plan: &ReleasePlan,
        leases: &LeaseStore,
        journal: &JournalStore,
        approvals: &ApprovalStore,
        runner: &mut R,
        gate: &mut G,
        probe: &mut P,
        executor: &mut E,
    ) -> Result<Vec<EffectOutcome>, OrchestrationError>
    where
        R: PhaseRunner,
        G: FirstPublicTriggerGate,
        P: CompletionProbe,
        E: IrreversibleExecutor,
    {
        ensure_matching_declaration(journal, plan)?;
        let train_lease = leases.acquire_train(plan.repository.clone(), plan.train.clone())?;
        if let Err(error) = self.registry.validate_plan(plan) {
            if let OrchestrationError::Refusal {
                phase: Some(phase), ..
            } = &error
            {
                journal.append_journal(JournalRecord::Refused {
                    phase: phase.clone(),
                    reason: error.to_string(),
                })?;
            }
            train_lease.release()?;
            return Err(error);
        }

        let mut outcomes = Vec::new();
        let mut approval: Option<ApprovalSubject> = None;

        for phase in &plan.phases {
            let class = self
                .registry
                .classify(&phase.phase_type)
                .expect("validated registry phase must remain registered");
            let repository_lease = if phase.tree_mutating {
                match leases.acquire_repository(plan.repository.clone()) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let error = OrchestrationError::Lease(error);
                        journal.append_journal(JournalRecord::Refused {
                            phase: phase.instance.clone(),
                            reason: error.to_string(),
                        })?;
                        train_lease.release()?;
                        return Err(error);
                    }
                }
            } else {
                None
            };
            journal.append_journal(JournalRecord::PhaseEntered {
                phase: phase.instance.clone(),
            })?;

            let phase_result = match class {
                PhaseClass::RefusalCapable | PhaseClass::PostBoundary => runner
                    .run(phase)
                    .map(|()| Vec::new())
                    .map_err(|error| match error {
                        PhaseExecutionError::Refusal {
                            code,
                            phase,
                            message,
                        } => OrchestrationError::PrecheckRefusal {
                            code,
                            phase,
                            message,
                        },
                        PhaseExecutionError::Seam(error) => OrchestrationError::Seam(error),
                    }),
                PhaseClass::Irreversible => (|| {
                    let subject = approval.get_or_insert_with(|| {
                        build_approval_subject(plan)
                            .expect("a plan with an irreversible phase has a public trigger")
                    });
                    match approvals.require_current(subject) {
                        Ok(_) => {}
                        Err(crate::approval::ApprovalError::NoCurrentApproval)
                        | Err(crate::approval::ApprovalError::SubjectMismatch) => {
                            approvals.invalidate_if_stale(subject)?;
                            let token = gate.confirm(subject)?;
                            approvals.persist_confirmed(subject.clone(), token)?;
                        }
                        Err(error) => return Err(error.into()),
                    }

                    let mut phase_outcomes = Vec::new();
                    for effect in effects_for_phase(plan, &phase.instance) {
                        let durable_approval = approvals.require_current(subject)?;
                        phase_outcomes.push(reconcile_effect(
                            plan,
                            journal,
                            &effect,
                            probe,
                            executor,
                            &durable_approval.subject,
                        )?);
                    }
                    Ok(phase_outcomes)
                })(),
            };

            let phase_result = match (phase_result, repository_lease) {
                (result, Some(lease)) => result.and_then(|outcomes| {
                    lease.release()?;
                    Ok(outcomes)
                }),
                (result, None) => result,
            };

            match phase_result {
                Ok(phase_outcomes) => {
                    let awaiting_probe = phase_outcomes
                        .iter()
                        .any(|outcome| matches!(outcome, EffectOutcome::AwaitingProbe));
                    if !awaiting_probe {
                        let evidence = phase_outcomes
                            .iter()
                            .filter_map(|outcome| match outcome {
                                EffectOutcome::Executed(evidence)
                                | EffectOutcome::Reconciled(evidence) => Some(evidence.clone()),
                                EffectOutcome::AwaitingProbe => None,
                            })
                            .collect();
                        journal.append_journal(JournalRecord::PhaseDone {
                            phase: phase.instance.clone(),
                            evidence,
                        })?;
                    }
                    outcomes.extend(phase_outcomes);
                }
                Err(error) => {
                    journal.append_journal(JournalRecord::Refused {
                        phase: phase.instance.clone(),
                        reason: error.to_string(),
                    })?;
                    return Err(error);
                }
            }
        }
        train_lease.release()?;
        Ok(outcomes)
    }
}

/// Reconciles or admits exactly one irreversible effect.
///
/// The executor runs only when no prior intent exists and the authoritative
/// probe reports the effect absent. Existing intents, including completed
/// journal history, require reconciliation instead of a duplicate call.
fn reconcile_effect<P, E>(
    plan: &ReleasePlan,
    journal: &JournalStore,
    effect: &PublicEffect,
    probe: &mut P,
    executor: &mut E,
    approval_subject: &ApprovalSubject,
) -> Result<EffectOutcome, OrchestrationError>
where
    P: CompletionProbe,
    E: IrreversibleExecutor,
{
    ensure_matching_declaration(journal, plan)?;
    let request = effect_request(plan, effect)?;
    let expected_identity = expected_identity(plan, effect)?;
    let attempted = attempted_intent(journal, &request)?;
    let completed = completion_exists(journal, &request)?;

    match probe.probe(&request)? {
        ProbeResult::Present(evidence) => {
            ensure_matching_evidence(&request, &expected_identity, &evidence)?;
            if !completed {
                if let Some(intent) = attempted {
                    journal.append_completion(&intent, evidence.clone())?;
                }
            }
            Ok(EffectOutcome::Reconciled(evidence))
        }
        ProbeResult::Undecidable(_) => Ok(EffectOutcome::AwaitingProbe),
        ProbeResult::Absent(evidence) => {
            if completed {
                return Err(OrchestrationError::refusal(
                    OrchestrationRefusalCode::ContradictoryEvidence,
                    Some(request.phase.clone()),
                    format!(
                        "done-probe reports `{}` absent although journal records its completion",
                        request.operation
                    ),
                ));
            }
            if attempted.is_some() {
                return Err(OrchestrationError::refusal(
                    OrchestrationRefusalCode::AttemptedIntentAbsent,
                    Some(request.phase.clone()),
                    format!(
                        "attempted operation `{}` is authoritatively absent; operator recovery is required",
                        request.operation
                    ),
                ));
            }
            ensure_matching_evidence(&request, &expected_identity, &evidence)?;
            let finalized_artifact = effect
                .artifact
                .as_ref()
                .and_then(|artifact| plan.finalized_artifact(artifact))
                .cloned();
            let admitted = AdmittedEffect::new(
                request.clone(),
                effect,
                finalized_artifact,
                approval_subject.clone(),
            )?;
            let (intent, evidence) =
                journal
                    .execute_with_intent(&admitted, executor)
                    .map_err(|error| match error {
                        StateError::Executor(error) => OrchestrationError::Seam(error),
                        StateError::ExecutorAdmission(error) => OrchestrationError::Executor(error),
                        error => OrchestrationError::State(error),
                    })?;
            ensure_matching_evidence(&request, &expected_identity, &evidence)?;
            journal.append_completion(&intent, evidence.clone())?;
            Ok(EffectOutcome::Executed(evidence))
        }
    }
}

/// Unfenced reconciliation harness for external acceptance tests.
///
/// Production callers must use [`Orchestrator::execute`], which acquires leases,
/// validates the complete plan, and reloads durable approval before every effect.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn reconcile_effect_unfenced_for_tests<P, E>(
    plan: &ReleasePlan,
    journal: &JournalStore,
    effect: &PublicEffect,
    probe: &mut P,
    executor: &mut E,
    approval_subject: &ApprovalSubject,
) -> Result<EffectOutcome, OrchestrationError>
where
    P: CompletionProbe,
    E: IrreversibleExecutor,
{
    reconcile_effect(plan, journal, effect, probe, executor, approval_subject)
}

fn ensure_matching_declaration(
    journal: &JournalStore,
    plan: &ReleasePlan,
) -> Result<(), OrchestrationError> {
    match journal.ensure_declaration_digest(&plan.declaration_digest) {
        Ok(()) => Ok(()),
        Err(StateError::DeclarationDigestMismatch { pinned, active }) => {
            let train_journal_id = journal.train_journal_id();
            Err(OrchestrationError::refusal(
                OrchestrationRefusalCode::DeclarationDigestMismatch,
                None,
                format!(
                    "active declaration digest `{active}` differs from pinned digest `{pinned}`; run `ck-release abandon {train_journal_id}` or `ck-release rebind {train_journal_id}` before resuming"
                ),
            ))
        }
        Err(error) => Err(error.into()),
    }
}

fn effects_for_phase(plan: &ReleasePlan, phase: &PhaseInstanceId) -> Vec<PublicEffect> {
    plan.public_effects
        .iter()
        .filter(|effect| effect.phase == *phase)
        .cloned()
        .collect()
}

fn effect_request(
    plan: &ReleasePlan,
    effect: &PublicEffect,
) -> Result<EffectRequest, OrchestrationError> {
    let artifact = effect
        .artifact
        .clone()
        .unwrap_or_else(|| ArtifactId::new("tag"));
    Ok(EffectRequest {
        repository: plan.repository.clone(),
        train: plan.train.clone(),
        phase: effect.phase.clone(),
        artifact,
        operation: effect.operation.clone(),
        intended_commit: plan.intended_commit.clone(),
        declaration_digest: plan.declaration_digest.clone(),
    })
}

fn expected_identity(
    plan: &ReleasePlan,
    effect: &PublicEffect,
) -> Result<String, OrchestrationError> {
    match &effect.artifact {
        Some(artifact) => plan
            .artifacts
            .iter()
            .find(|candidate| candidate.artifact == *artifact)
            .map(|artifact| artifact.identity.clone())
            .ok_or_else(|| {
                OrchestrationError::refusal(
                    OrchestrationRefusalCode::MissingPublicEffect,
                    Some(effect.phase.clone()),
                    format!(
                        "public effect `{}` references an unknown artifact",
                        effect.operation
                    ),
                )
            }),
        None => Ok(plan.intended_commit.to_string()),
    }
}

fn attempted_intent(
    journal: &JournalStore,
    request: &EffectRequest,
) -> Result<Option<PendingIntent>, OrchestrationError> {
    Ok(journal
        .read_intents()?
        .into_iter()
        .map(|record| match record {
            IntentRecord::Pending(intent) => intent,
        })
        .find(|intent| intent_matches(intent, request)))
}

fn completion_exists(
    journal: &JournalStore,
    request: &EffectRequest,
) -> Result<bool, OrchestrationError> {
    Ok(journal.read_journal()?.iter().any(|record| match record {
        JournalRecord::Completion { intent, .. } => intent_matches(intent, request),
        JournalRecord::DeclarationPinned { .. }
        | JournalRecord::DeclarationRebound { .. }
        | JournalRecord::PhaseEntered { .. }
        | JournalRecord::PhaseDone { .. }
        | JournalRecord::Refused { .. }
        | JournalRecord::WorkingTreeMutation { .. }
        | JournalRecord::ResidueSwept { .. }
        | JournalRecord::Terminalized { .. } => false,
    }))
}

fn intent_matches(intent: &PendingIntent, request: &EffectRequest) -> bool {
    intent.train == request.train
        && intent.phase == request.phase
        && intent.artifact == request.artifact
        && intent.operation == request.operation
        && intent.intended_commit == request.intended_commit
        && intent.declaration_digest == request.declaration_digest
}

fn ensure_matching_evidence(
    request: &EffectRequest,
    expected_identity: &str,
    evidence: &ProbeEvidence,
) -> Result<(), OrchestrationError> {
    if evidence.identity == expected_identity {
        return Ok(());
    }
    Err(OrchestrationError::refusal(
        OrchestrationRefusalCode::ContradictoryEvidence,
        Some(request.phase.clone()),
        format!(
            "operation `{}` expected identity `{expected_identity}`, observed `{}`",
            request.operation, evidence.identity
        ),
    ))
}

/// Returns the public operations that can ever be admitted by this module.
pub fn admitted_operations(plan: &ReleasePlan) -> HashSet<OperationId> {
    plan.public_effects
        .iter()
        .map(|effect| effect.operation.clone())
        .collect()
}
