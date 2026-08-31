//! Effect-asserted coverage for declared local command gates.

#[path = "support/mod.rs"]
mod support;

use cortexkit_release::{
    approval::{ApprovalStore, ApprovalSubject},
    declaration::{parse, DeclarationRefusalCode},
    lease::LeaseStore,
    orchestrator::{FirstPublicTriggerGate, OrchestrationError, Orchestrator},
    phases::command::CommandPhaseRunner,
    plan::build_dry_run_plan,
    state::{JournalRecord, JournalStore, TrainJournalIdentity},
    ApprovalToken, CompletionProbe, IrreversibleExecutor, ProbeEvidence, ProbeResult, RepositoryId,
    SeamError,
};
use std::{fs, path::Path};
use tempfile::TempDir;

struct NeverGate;

impl FirstPublicTriggerGate for NeverGate {
    fn confirm(&mut self, _: &ApprovalSubject) -> Result<ApprovalToken, SeamError> {
        Err(SeamError::new(
            "local command gate test reached an unexpected public trigger",
        ))
    }
}

struct NeverProbe;

impl CompletionProbe for NeverProbe {
    fn probe(&mut self, _: &cortexkit_release::EffectRequest) -> Result<ProbeResult, SeamError> {
        Err(SeamError::new(
            "local command gate test reached an unexpected public-effect probe",
        ))
    }
}

struct NeverExecutor;

impl IrreversibleExecutor for NeverExecutor {
    fn execute(
        &mut self,
        _: &cortexkit_release::executor::AdmittedEffect,
    ) -> Result<ProbeEvidence, SeamError> {
        Err(SeamError::new(
            "local command gate test reached an unexpected public-effect executor",
        ))
    }
}

struct Execution {
    repository: support::MintedRepo,
    _state_home: TempDir,
    journal: JournalStore,
    result: Result<Vec<cortexkit_release::orchestrator::EffectOutcome>, OrchestrationError>,
}

fn declaration(phases: &str) -> String {
    format!(
        r#"{{
          "version": 1,
          "trains": [{{
            "id": "local-gates",
            "intended_commit": "gates-commit",
            "signing_profile": "none",
            "phases": [{phases}]
          }}]
        }}"#
    )
}

fn execute(declaration: &str, setup: impl FnOnce(&Path)) -> Execution {
    let repository =
        support::MintedRepo::mint_with_declaration(support::RepositoryShape::Valid, declaration)
            .unwrap();
    setup(repository.path());
    let parsed = parse(declaration).unwrap();
    let plan = build_dry_run_plan(
        RepositoryId::new("command-phase-runner"),
        &parsed,
        "local-gates",
        &[],
    )
    .unwrap();
    let state_home = tempfile::tempdir().unwrap();
    let identity = TrainJournalIdentity::new(
        plan.repository.clone(),
        plan.train.clone(),
        "command-phase-runner",
    )
    .unwrap();
    let journal = JournalStore::new(state_home.path(), identity.clone()).unwrap();
    journal.pin_declaration(&parsed).unwrap();
    let approvals = ApprovalStore::new(state_home.path(), identity).unwrap();
    let leases = LeaseStore::new(state_home.path()).unwrap();
    let mut runner = CommandPhaseRunner::new(repository.path(), &journal);
    let mut gate = NeverGate;
    let mut probe = NeverProbe;
    let mut executor = NeverExecutor;
    let result = Orchestrator::default().execute(
        &plan,
        &leases,
        &journal,
        &approvals,
        &mut runner,
        &mut gate,
        &mut probe,
        &mut executor,
    );

    Execution {
        repository,
        _state_home: state_home,
        journal,
        result,
    }
}

fn command_attempts(records: &[JournalRecord]) -> Vec<(String, u64, Option<i32>, String, String)> {
    records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::LocalCommandAttempt {
                phase,
                attempt,
                exit_code,
                output_path,
                load_class,
            } => Some((
                phase.to_string(),
                *attempt,
                *exit_code,
                output_path.to_string_lossy().into_owned(),
                load_class.clone(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn gates_local_legs_execute_in_declaration_order_and_attach_output_evidence() {
    let execution = execute(
        &declaration(
            r#"{"id":"first","type":"gates_local","params":{"command":"sh","args":["-c","printf 'first\n' >> gate-order; printf first-output"],"load_class":"cpu"}},
               {"id":"second","type":"gates_local","params":{"command":"sh","args":["-c","printf 'second\n' >> gate-order; printf second-output"],"load_class":"io"}}"#,
        ),
        |_| {},
    );

    assert!(execution.result.is_ok(), "{:?}", execution.result);
    assert_eq!(
        fs::read_to_string(execution.repository.path().join("gate-order")).unwrap(),
        "first\nsecond\n"
    );
    let records = execution.journal.read_journal().unwrap();
    let attempts = command_attempts(&records);
    assert_eq!(
        attempts
            .iter()
            .map(|(phase, attempt, exit_code, _, load_class)| (
                phase.as_str(),
                *attempt,
                *exit_code,
                load_class.as_str()
            ))
            .collect::<Vec<_>>(),
        [("first", 1, Some(0), "cpu"), ("second", 1, Some(0), "io")]
    );
    for (_, _, _, output_path, _) in &attempts {
        assert!(Path::new(output_path).exists());
    }
    assert_eq!(fs::read_to_string(&attempts[0].3).unwrap(), "first-output");
    assert_eq!(fs::read_to_string(&attempts[1].3).unwrap(), "second-output");
    let completed_output_paths = records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::PhaseDone { evidence, .. } => Some(
                evidence
                    .iter()
                    .map(|evidence| evidence.reference.as_str())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completed_output_paths,
        vec![vec![attempts[0].3.as_str()], vec![attempts[1].3.as_str()]]
    );
}

#[test]
fn gates_local_retries_a_load_classified_leg_with_fresh_output_artifacts() {
    let execution = execute(
        &declaration(
            r#"{"id":"load-gate","type":"gates_local","params":{"command":"sh","args":["-c","if test -e retry-once; then printf recovered; else : > retry-once; printf transient >&2; exit 75; fi"],"retry_budget":1,"load_class":"shared-host-contention"}}"#,
        ),
        |_| {},
    );

    assert!(execution.result.is_ok(), "{:?}", execution.result);
    let records = execution.journal.read_journal().unwrap();
    let attempts = command_attempts(&records);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].0, "load-gate");
    assert_eq!(attempts[0].1, 1);
    assert_eq!(attempts[0].2, Some(75));
    assert_eq!(attempts[0].4, "shared-host-contention");
    assert_eq!(attempts[1].1, 2);
    assert_eq!(attempts[1].2, Some(0));
    assert_ne!(attempts[0].3, attempts[1].3);
    assert_eq!(fs::read_to_string(&attempts[0].3).unwrap(), "transient");
    assert_eq!(fs::read_to_string(&attempts[1].3).unwrap(), "recovered");
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::PhaseDone { phase, evidence }
            if phase.as_str() == "load-gate" && evidence.len() == 2
    )));
}

#[test]
fn gates_local_exhaustion_refuses_before_a_later_phase_is_entered() {
    let execution = execute(
        &declaration(
            r#"{"id":"flaky","type":"gates_local","params":{"command":"sh","args":["-c","printf failed; exit 19"],"retry_budget":1,"load_class":"shared-host-contention"}},
               {"id":"later","type":"gates_local","params":{"command":"sh","args":["-c","printf entered > later-entered"],"load_class":"cpu"}}"#,
        ),
        |_| {},
    );

    let error = execution.result.unwrap_err().to_string();
    assert!(error.contains("flaky"));
    assert!(error.contains("attempt 1 exited 19"));
    assert!(error.contains("attempt 2 exited 19"));
    let records = execution.journal.read_journal().unwrap();
    assert_eq!(command_attempts(&records).len(), 2);
    assert!(records.iter().all(|record| !matches!(
        record,
        JournalRecord::PhaseEntered { phase } if phase.as_str() == "later"
    )));
    assert!(!execution.repository.path().join("later-entered").exists());
}

#[test]
fn malformed_gates_local_parameters_refuse_before_an_output_artifact_can_exist() {
    for declaration in [
        declaration(
            r#"{"id":"unknown","type":"gates_local","params":{"command":"sh","args":["-c","touch spawned"],"load_class":"cpu","unexpected":true}}"#,
        ),
        declaration(
            r#"{"id":"missing-command","type":"gates_local","params":{"args":["-c","touch spawned"],"load_class":"cpu"}}"#,
        ),
    ] {
        let repository = support::MintedRepo::mint_with_declaration(
            support::RepositoryShape::Valid,
            &declaration,
        )
        .unwrap();
        let error = parse(&declaration).unwrap_err();
        assert_eq!(error.code, DeclarationRefusalCode::InvalidPhaseParameters);
        assert!(!repository.path().join("spawned").exists());
        let state_home = tempfile::tempdir().unwrap();
        let journal = JournalStore::new(
            state_home.path(),
            TrainJournalIdentity::new(
                RepositoryId::new("malformed-command"),
                "local-gates".into(),
                "malformed-command",
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!journal.evidence_dir().exists());
    }
}

#[test]
fn gates_local_resolves_relative_cwd_and_adds_declared_environment() {
    // The cwd arm asserts by EFFECT (the child drops a marker file into its
    // working directory) rather than by comparing path renderings: a shell's
    // $PWD on Windows runners is a Git-Bash POSIX rendering while the parent
    // sees native (or \\?\-extended) paths, so string equality over the same
    // directory fails across platforms even when the resolution is correct.
    let execution = execute(
        &declaration(
            r#"{"id":"environment","type":"gates_local","params":{"command":"sh","args":["-c","printf '%s' \"$GATES_LOCAL_TEST_ENV\" > cwd-proof.txt; printf 'ran|%s' \"$GATES_LOCAL_TEST_ENV\""],"cwd":"child","env":{"GATES_LOCAL_TEST_ENV":"declared-value"},"load_class":"cpu"}}"#,
        ),
        |repository| fs::create_dir(repository.join("child")).unwrap(),
    );

    assert!(execution.result.is_ok(), "{:?}", execution.result);
    let attempts = command_attempts(&execution.journal.read_journal().unwrap());
    assert_eq!(attempts.len(), 1);
    // Env arm: the declared variable reached the child (captured output).
    assert_eq!(
        fs::read_to_string(&attempts[0].3).unwrap(),
        "ran|declared-value"
    );
    // Cwd arm: the marker landed inside the declared relative cwd, with the
    // declared env value as its content (one file proves both resolutions).
    assert_eq!(
        fs::read_to_string(
            execution
                .repository
                .path()
                .join("child")
                .join("cwd-proof.txt")
        )
        .unwrap(),
        "declared-value"
    );
}
