use super::*;
use crate::{
    approval::{build_approval_subject, ApprovalStore, ApprovalSubject},
    declaration::parse,
    executor::AdmittedEffect,
    orchestrator::{
        reconcile_effect_unfenced_for_tests, OrchestrationError, OrchestrationRefusalCode,
    },
    plan::{build_dry_run_plan, FinalizedArtifact, PublicEffect, ReleaseIdentity, ReleasePlan},
    state::{IntentRecord, JournalRecord, TrainJournalIdentity},
    ApprovalToken, ArtifactId, CommitId, CompletionProbe, EffectRequest, IrreversibleExecutor,
    OperationId, PhaseInstanceId, ProbeEvidence, ProbeResult, RepositoryId, SeamError, TrainId,
};
use std::{cell::Cell, fs};
use tempfile::TempDir;

const PINNED_DECLARATION: &str = r#"
{
  "version": 1,
  "trains": [{
    "id": "release",
    "intended_commit": "commit-a",
    "tag": "v1.2.3",
    "signing_profile": "none",
    "operator_gates": ["first_public_trigger"],
    "artifacts": [{"id": "archive", "kind": "archive", "identity_channel": "asset_sha256"}],
    "phases": [{"id": "preflight", "type": "preflight"}, {"id": "publish", "type": "publish"}]
  }]
}
"#;

const REPLACEMENT_DECLARATION: &str = r#"
{
  "version": 1,
  "trains": [{
    "id": "release",
    "intended_commit": "commit-b",
    "tag": "v1.2.3",
    "signing_profile": "none",
    "operator_gates": ["first_public_trigger"],
    "artifacts": [{"id": "archive", "kind": "archive", "identity_channel": "asset_sha256"}],
    "phases": [{"id": "preflight", "type": "preflight"}, {"id": "publish", "type": "publish"}]
  }]
}
"#;

fn pinned_declaration() -> crate::declaration::ParsedDeclaration {
    parse(PINNED_DECLARATION).unwrap()
}

fn replacement_declaration() -> crate::declaration::ParsedDeclaration {
    parse(REPLACEMENT_DECLARATION).unwrap()
}

fn stores() -> (TempDir, JournalStore, ApprovalStore) {
    let root = tempfile::tempdir().unwrap();
    let identity = TrainJournalIdentity::new(
        RepositoryId::new("example-repository"),
        TrainId::new("release"),
        "run-1",
    )
    .unwrap();
    let journal = JournalStore::new(root.path(), identity.clone()).unwrap();
    let approvals = ApprovalStore::new(root.path(), identity).unwrap();
    (root, journal, approvals)
}

fn pin(journal: &JournalStore) -> crate::declaration::ParsedDeclaration {
    let declaration = pinned_declaration();
    journal.pin_declaration(&declaration).unwrap();
    declaration
}

fn request() -> EffectRequest {
    EffectRequest {
        repository: RepositoryId::new("example-repository"),
        train: TrainId::new("release"),
        phase: PhaseInstanceId::new("publish"),
        artifact: ArtifactId::new("archive"),
        operation: OperationId::new("publish:archive"),
        intended_commit: CommitId::new("commit-a"),
        declaration_digest: pinned_declaration().digest,
    }
}

fn durable_subject() -> crate::ApprovalSubject {
    crate::ApprovalSubject {
        repository: RepositoryId::new("example-repository"),
        train: TrainId::new("release"),
        intended_commit: CommitId::new("commit-a"),
        declaration_digest: pinned_declaration().digest,
        artifact_digests: Vec::new(),
        public_effects: vec![OperationId::new("publish:archive")],
    }
}

fn plan(declaration: &crate::declaration::ParsedDeclaration) -> ReleasePlan {
    build_dry_run_plan(
        RepositoryId::new("example-repository"),
        declaration,
        "release",
        &[FinalizedArtifact {
            artifact: ArtifactId::new("archive"),
            identity: "archive-v1.2.3".to_owned(),
            bytes: b"final archive bytes".to_vec(),
        }],
    )
    .unwrap()
}

fn approval_subject() -> ApprovalSubject {
    ApprovalSubject {
        repository: RepositoryId::new("example-repository"),
        train: TrainId::new("release"),
        intended_commit: CommitId::new("commit-a"),
        declaration_digest: pinned_declaration().digest,
        artifacts: Vec::new(),
        version_or_run_id: ReleaseIdentity::Version("v1.2.3".to_owned()),
        public_effects: vec![PublicEffect {
            phase: PhaseInstanceId::new("publish"),
            operation: OperationId::new("publish:archive"),
            artifact: Some(ArtifactId::new("archive")),
        }],
    }
}

#[test]
fn digest_mismatch_refuses_before_credentials_or_phases_and_names_both_ceremonies() {
    let (_root, journal, _approvals) = stores();
    let _pinned = pin(&journal);
    let active = replacement_declaration();
    let downstream_calls = Cell::new(0);

    let result = resume(&journal, &active, || {
        downstream_calls.set(downstream_calls.get() + 1);
        Ok(())
    });

    let CeremonyError::Refusal(refusal) = result.unwrap_err() else {
        panic!("mismatched declaration must return a typed refusal");
    };
    assert_eq!(refusal.code, CeremonyRefusalCode::DeclarationDigestMismatch);
    assert_eq!(downstream_calls.get(), 0);
    assert!(refusal.message.contains("ck-release abandon release-run-1"));
    assert!(refusal.message.contains("ck-release rebind release-run-1"));
}

struct CountingProbe(Cell<usize>);

impl CompletionProbe for CountingProbe {
    fn probe(&mut self, _: &EffectRequest) -> Result<ProbeResult, SeamError> {
        self.0.set(self.0.get() + 1);
        Ok(ProbeResult::Absent(ProbeEvidence::default()))
    }
}

struct CountingExecutor(Cell<usize>);

impl IrreversibleExecutor for CountingExecutor {
    fn execute(&mut self, _: &AdmittedEffect) -> Result<ProbeEvidence, SeamError> {
        self.0.set(self.0.get() + 1);
        Ok(ProbeEvidence::default())
    }
}

#[test]
fn direct_reconciliation_refuses_digest_mismatch_before_any_provider_call() {
    let (_root, journal, _approvals) = stores();
    let pinned = pin(&journal);
    let active = replacement_declaration();
    let active_plan = plan(&active);
    let effect = active_plan
        .public_effects
        .iter()
        .find(|effect| effect.artifact.is_some())
        .unwrap();
    let subject = build_approval_subject(&active_plan).unwrap();
    let mut probe = CountingProbe(Cell::new(0));
    let mut executor = CountingExecutor(Cell::new(0));

    let result = reconcile_effect_unfenced_for_tests(
        &active_plan,
        &journal,
        effect,
        &mut probe,
        &mut executor,
        &subject,
    );

    assert!(matches!(
        result,
        Err(OrchestrationError::Refusal {
            code: OrchestrationRefusalCode::DeclarationDigestMismatch,
            ..
        })
    ));
    assert_ne!(pinned.digest, active.digest);
    assert_eq!(probe.0.get(), 0);
    assert_eq!(executor.0.get(), 0);
}

#[test]
fn abandonment_terminalizes_the_journal_without_deleting_evidence() {
    let (_root, journal, _approvals) = stores();
    pin(&journal);
    let intent = journal
        .append_intent(&request(), durable_subject())
        .unwrap();
    journal
        .append_completion(
            &intent,
            ProbeEvidence {
                reference: "provider/archive".to_owned(),
                identity: "archive-v1.2.3".to_owned(),
            },
        )
        .unwrap();
    let records_before = journal.read_journal().unwrap();
    let intents_before = journal.read_intents().unwrap();

    let state = abandon(&journal).unwrap();

    assert!(matches!(state, TrainTerminalState::Abandoned { .. }));
    let records_after = journal.read_journal().unwrap();
    assert_eq!(
        &records_after[..records_before.len()],
        records_before.as_slice()
    );
    assert!(matches!(
        records_after.last(),
        Some(JournalRecord::Terminalized {
            state: TrainTerminalState::Abandoned { .. }
        })
    ));
    assert_eq!(journal.read_intents().unwrap(), intents_before);
    assert!(matches!(
        journal.read_intents().unwrap().as_slice(),
        [IntentRecord::Pending(_)]
    ));

    let downstream_calls = Cell::new(0);
    assert!(matches!(
        resume(&journal, &pinned_declaration(), || {
            downstream_calls.set(downstream_calls.get() + 1);
            Ok(())
        }),
        Err(CeremonyError::Refusal(CeremonyRefusal {
            code: CeremonyRefusalCode::TrainAbandoned,
            ..
        }))
    ));
    assert_eq!(downstream_calls.get(), 0);
}

#[test]
fn unconfirmed_rebind_changes_no_durable_journal_or_approval_state() {
    let (_root, journal, approvals) = stores();
    let pinned = pin(&journal);
    approvals
        .persist_confirmed(approval_subject(), ApprovalToken::new("old-approval"))
        .unwrap();
    let journal_before = fs::read(journal.journal_path()).unwrap();
    let approval_before = fs::read(approvals.approval_path()).unwrap();
    let preview = prepare_rebind(&journal, &replacement_declaration()).unwrap();

    assert!(preview.render_diff().contains("/trains/0/intended_commit"));
    assert!(matches!(
        confirm_rebind(&journal, &approvals, preview, RebindConfirmation::Declined),
        Err(CeremonyError::Refusal(CeremonyRefusal {
            code: CeremonyRefusalCode::RebindConfirmationRequired,
            ..
        }))
    ));

    assert_eq!(fs::read(journal.journal_path()).unwrap(), journal_before);
    assert_eq!(
        fs::read(approvals.approval_path()).unwrap(),
        approval_before
    );
    assert_eq!(
        journal.pinned_declaration().unwrap().unwrap().digest,
        pinned.digest
    );
}

#[test]
fn confirmed_rebind_pins_the_replacement_and_requires_approval_reconstruction() {
    let (_root, journal, approvals) = stores();
    pin(&journal);
    approvals
        .persist_confirmed(approval_subject(), ApprovalToken::new("old-approval"))
        .unwrap();
    let replacement = replacement_declaration();
    let preview = prepare_rebind(&journal, &replacement).unwrap();

    let outcome =
        confirm_rebind(&journal, &approvals, preview, RebindConfirmation::Confirmed).unwrap();

    assert_eq!(outcome.binding.digest, replacement.digest);
    assert!(outcome.invalidated_approval);
    assert!(outcome.approval_reconstruction_required);
    assert!(approvals.load().unwrap().is_none());
    assert!(matches!(
        journal.read_journal().unwrap().last(),
        Some(JournalRecord::DeclarationRebound { .. })
    ));
    let resumed = Cell::new(0);
    resume(&journal, &replacement, || {
        resumed.set(resumed.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(resumed.get(), 1);
}
