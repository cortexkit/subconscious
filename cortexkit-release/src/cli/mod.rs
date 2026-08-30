//! Command-line rendering for the `ck-release` release machine.
//!
//! The CLI owns argument parsing and versioned output only. Planning and status
//! deliberately construct no credential-bearing provider, while execution uses
//! the hermetic synthetic provider solely when a caller explicitly requests it.

// CLI failures flow once per process toward the exit path; boxing the error
// to satisfy result_large_err would touch every construction site to save
// bytes freed microseconds later at exit.
#![allow(clippy::result_large_err)]
use crate::{
    approval::{build_approval_subject, ApprovalError, ApprovalStore, ApprovalSubject},
    ceremony::{self, CeremonyError, RebindConfirmation},
    declaration::{
        self, DeclarationError, DeclarationRefusalCode, ParsedDeclaration, TrainDeclaration,
    },
    lease::LeaseError,
    orchestrator::{
        EffectOutcome, FirstPublicTriggerGate, OrchestrationError, OrchestrationRefusalCode,
        Orchestrator, PhaseRunner,
    },
    plan::{self, FinalizedArtifact, PlanError, ReleasePlan},
    state::{self, JournalRecord, JournalStore, StateError, TrainJournalIdentity},
    ApprovalToken, ArtifactId, CompletionProbe, EffectRequest, IrreversibleExecutor, ProbeEvidence,
    ProbeResult, RepositoryId, SeamError,
};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

/// The stable schema version for all machine-readable command responses.
pub const MACHINE_OUTPUT_VERSION: u32 = 1;
const REFUSAL_EXIT: u8 = 2;

#[derive(Parser)]
#[command(
    name = "ck-release",
    about = "Journaled release state machine with explicit operator boundaries"
)]
struct Cli {
    /// Emit the versioned response envelope consumed by automation.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report the normalized declaration and its digest without side effects.
    Declare(RepositoryArgs),
    /// Validate declaration semantics without contacting a provider.
    Validate(ValidateArgs),
    /// Resolve a credential-free release plan.
    Plan(PlanArgs),
    /// Create or continue a train and execute it through an admitted provider.
    Execute(ExecutionArgs),
    /// Reconcile durable intents after checking the pinned declaration digest.
    Resume(ExecutionArgs),
    /// Report durable train state without contacting a provider.
    Status(StatusArgs),
    /// Terminalize a journal while retaining all prior records as evidence.
    Abandon(CeremonyArgs),
    /// Display and explicitly confirm a declaration-digest replacement.
    Rebind(RebindArgs),
}

#[derive(Args, Clone)]
struct RepositoryArgs {
    /// Repository containing `.cortexkit/release.jsonc`; defaults to the current directory.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct ValidateArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Validate one named train in addition to the complete declaration.
    #[arg(long)]
    train: Option<String>,
}

#[derive(Args)]
struct PlanArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Declared train to plan.
    #[arg(long)]
    train: String,
    /// Make explicit that planning is a no-provider dry run.
    #[arg(long)]
    dry_run: bool,
    /// Final artifact material as `<artifact-id>=<path>`; repeat once per artifact.
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
}

#[derive(Args)]
struct ExecutionArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Declared train to execute or resume.
    #[arg(long)]
    train: String,
    /// Final artifact material as `<artifact-id>=<path>`; repeat once per artifact.
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
    /// Explicitly confirm the complete approval subject at the first public trigger.
    #[arg(long)]
    confirm_first_public_trigger: bool,
    /// Use the durable, hermetic provider fake. This exists for synthetic training only.
    #[arg(long)]
    synthetic_provider: bool,
    /// Stop after the synthetic provider's effect and before completion is journaled.
    #[arg(long, requires = "synthetic_provider")]
    interrupt_after_effect: bool,
}

#[derive(Args)]
struct StatusArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Declared train whose durable state should be reported.
    #[arg(long)]
    train: String,
}

#[derive(Args)]
struct CeremonyArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Declared train whose journal should be terminalized.
    train: String,
}

#[derive(Args)]
struct RebindArgs {
    #[command(flatten)]
    repository: RepositoryArgs,
    /// Declared train whose pinned declaration should be replaced.
    train: String,
    /// The replacement digest printed by a prior `rebind` preview.
    #[arg(long, value_name = "DIGEST")]
    confirm: Option<String>,
}

#[derive(Debug, Serialize)]
struct MachineResponse {
    version: u32,
    command: String,
    outcome: &'static str,
    data: Value,
}

#[derive(Serialize)]
struct MachineFailureResponse {
    version: u32,
    command: String,
    outcome: &'static str,
    error: FailureDetail,
}

#[derive(Debug, Serialize)]
struct FailureDetail {
    code: String,
    message: String,
    context: Value,
}

#[derive(Clone, Copy, Debug)]
enum FailureClass {
    Refusal,
    Internal,
}

#[derive(Debug)]
struct CliFailure {
    class: FailureClass,
    command: String,
    detail: FailureDetail,
}

impl CliFailure {
    fn refusal(
        command: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::refusal_with_context(command, code, message, Value::Object(Default::default()))
    }

    fn refusal_with_context(
        command: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        context: Value,
    ) -> Self {
        Self {
            class: FailureClass::Refusal,
            command: command.into(),
            detail: FailureDetail {
                code: code.into(),
                message: message.into(),
                context,
            },
        }
    }

    fn internal(
        command: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class: FailureClass::Internal,
            command: command.into(),
            detail: FailureDetail {
                code: code.into(),
                message: message.into(),
                context: Value::Object(Default::default()),
            },
        }
    }
}

struct PreparedRepository {
    id: RepositoryId,
    declaration: ParsedDeclaration,
}

/// Runs the production CLI, using the single ruled per-user journal root.
pub fn run() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    if is_help_or_version_request(&arguments) {
        return render_clap_help_or_version(&arguments);
    }

    let wants_json = arguments
        .iter()
        .any(|argument| argument.to_str() == Some("--json"));
    let parsed = Cli::try_parse_from(&arguments).map_err(|error| {
        if arguments
            .iter()
            .any(|argument| argument.to_str() == Some("place"))
        {
            CliFailure::refusal(
                "place",
                "machine_owned_place_absent",
                "placement is an external operator ceremony; ck-release has no `place` action",
            )
        } else {
            CliFailure::refusal("ck-release", "invalid_command", error.to_string())
        }
    });

    match parsed.and_then(|cli| execute(cli, None)) {
        Ok(response) => render_success(wants_json, &response),
        Err(failure) => render_failure(wants_json, &failure),
    }
}

fn is_help_or_version_request(arguments: &[OsString]) -> bool {
    arguments.iter().any(|argument| {
        matches!(
            argument.to_str(),
            Some("--help") | Some("-h") | Some("--version") | Some("-V")
        )
    })
}

fn render_clap_help_or_version(arguments: &[OsString]) -> ExitCode {
    match Cli::try_parse_from(arguments) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = error.print();
            ExitCode::SUCCESS
        }
    }
}

fn render_success(json_output: bool, response: &MachineResponse) -> ExitCode {
    if json_output {
        match serde_json::to_string(response) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => {
                eprintln!("ck-release could not render machine output: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("{}: {}", response.command, response.outcome);
        match serde_json::to_string_pretty(&response.data) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => {
                eprintln!("ck-release could not render command data: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn render_failure(json_output: bool, failure: &CliFailure) -> ExitCode {
    if json_output {
        let response = MachineFailureResponse {
            version: MACHINE_OUTPUT_VERSION,
            command: failure.command.clone(),
            outcome: match failure.class {
                FailureClass::Refusal => "refused",
                FailureClass::Internal => "failed",
            },
            error: FailureDetail {
                code: failure.detail.code.clone(),
                message: failure.detail.message.clone(),
                context: failure.detail.context.clone(),
            },
        };
        match serde_json::to_string(&response) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => eprintln!("ck-release could not render machine failure: {error}"),
        }
    } else {
        eprintln!(
            "{}: {}: {}",
            failure.command, failure.detail.code, failure.detail.message
        );
    }
    match failure.class {
        FailureClass::Refusal => ExitCode::from(REFUSAL_EXIT),
        FailureClass::Internal => ExitCode::FAILURE,
    }
}

fn execute(cli: Cli, state_root: Option<PathBuf>) -> Result<MachineResponse, CliFailure> {
    let command_name = command_name(&cli.command);
    let state_root = match state_root {
        Some(path) => path,
        None => {
            state::default_state_root().map_err(|error| map_state_error(command_name, error))?
        }
    };

    match cli.command {
        Command::Declare(arguments) => declare(arguments),
        Command::Validate(arguments) => validate(arguments),
        Command::Plan(arguments) => plan(arguments),
        Command::Execute(arguments) => execute_train(arguments, state_root, false),
        Command::Resume(arguments) => execute_train(arguments, state_root, true),
        Command::Status(arguments) => status(arguments, state_root),
        Command::Abandon(arguments) => abandon(arguments, state_root),
        Command::Rebind(arguments) => rebind(arguments, state_root),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Declare(_) => "declare",
        Command::Validate(_) => "validate",
        Command::Plan(_) => "plan",
        Command::Execute(_) => "execute",
        Command::Resume(_) => "resume",
        Command::Status(_) => "status",
        Command::Abandon(_) => "abandon",
        Command::Rebind(_) => "rebind",
    }
}

fn success(command: &str, data: Value) -> MachineResponse {
    MachineResponse {
        version: MACHINE_OUTPUT_VERSION,
        command: command.to_owned(),
        outcome: "ok",
        data,
    }
}

fn declare(arguments: RepositoryArgs) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("declare", &arguments)?;
    let trains = prepared
        .declaration
        .declaration
        .trains
        .iter()
        .map(|train| {
            json!({
                "id": train.id,
                "journal_id": journal_id_for(train),
                "intended_commit": train.intended_commit,
                "phases": train.phases.iter().map(|phase| json!({"id": phase.id, "type": phase.phase_type})).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    Ok(success(
        "declare",
        json!({
            "repository": prepared.id,
            "declaration_digest": prepared.declaration.digest,
            "trains": trains,
        }),
    ))
}

fn validate(arguments: ValidateArgs) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("validate", &arguments.repository)?;
    let selected_train = arguments
        .train
        .as_deref()
        .map(|train| {
            find_train("validate", &prepared.declaration, train)
                .map(|candidate| candidate.id.clone())
        })
        .transpose()?;
    Ok(success(
        "validate",
        json!({
            "repository": prepared.id,
            "declaration_digest": prepared.declaration.digest,
            "selected_train": selected_train,
            "validated_train_count": prepared.declaration.declaration.trains.len(),
            "provider_accessed": false,
        }),
    ))
}

fn plan(arguments: PlanArgs) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("plan", &arguments.repository)?;
    let plan = build_plan("plan", &prepared, &arguments.train, &arguments.artifacts)?;
    Ok(success(
        "plan",
        json!({
            "dry_run": arguments.dry_run,
            "provider_accessed": false,
            "release_plan": plan,
            "approval_subject": build_approval_subject(&plan).ok(),
        }),
    ))
}

fn execute_train(
    arguments: ExecutionArgs,
    state_root: PathBuf,
    is_resume: bool,
) -> Result<MachineResponse, CliFailure> {
    let command = if is_resume { "resume" } else { "execute" };
    let prepared = load_repository(command, &arguments.repository)?;
    let plan = build_plan(command, &prepared, &arguments.train, &arguments.artifacts)?;
    let identity = journal_identity(command, &prepared, &arguments.train)?;
    let journal = JournalStore::new(&state_root, identity.clone())
        .map_err(|error| map_state_error(command, error))?;

    if is_resume {
        ceremony::admit_resume(&journal, &prepared.declaration)
            .map_err(|error| map_ceremony_error(command, error))?;
    } else {
        journal
            .pin_declaration(&prepared.declaration)
            .map_err(|error| map_state_error(command, error))?;
    }

    let approvals = ApprovalStore::new(&state_root, identity)
        .map_err(|error| map_approval_error(command, error))?;
    let approval_subject = build_approval_subject(&plan).ok();
    if !plan.public_effects.is_empty() && !arguments.synthetic_provider {
        return Err(CliFailure::refusal_with_context(
            command,
            "provider_not_configured",
            "this build has no production executor wired; synthetic execution requires --synthetic-provider",
            json!({"approval_subject": approval_subject}),
        ));
    }
    if let Some(subject) = &approval_subject {
        let already_approved = approvals.require_current(subject).is_ok();
        if !already_approved && !arguments.confirm_first_public_trigger {
            return Err(CliFailure::refusal_with_context(
                command,
                "approval_required",
                "confirm the exact approval subject before the first public trigger",
                json!({"approval_subject": subject}),
            ));
        }
    }

    let mut runner = NoopPhaseRunner;
    let mut gate = SyntheticApprovalGate {
        confirmed: arguments.confirm_first_public_trigger,
    };
    let mut probe = SyntheticProvider::new(&state_root, &plan, false)
        .map_err(|error| map_synthetic_error(command, error))?;
    let mut executor = SyntheticProvider::new(&state_root, &plan, arguments.interrupt_after_effect)
        .map_err(|error| map_synthetic_error(command, error))?;
    let leases = crate::lease::LeaseStore::new(&state_root)
        .map_err(|error| map_lease_error(command, error))?;
    let outcomes = Orchestrator::default()
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
        .map_err(|error| map_orchestration_error(command, error))?;
    let pending_intents = journal
        .pending_intents()
        .map_err(|error| map_state_error(command, error))?;
    let completed = pending_intents.is_empty()
        && outcomes
            .iter()
            .all(|outcome| !matches!(outcome, EffectOutcome::AwaitingProbe));
    let phases = phase_state(&prepared.declaration, &arguments.train, &journal, command)?;

    Ok(success(
        command,
        json!({
            "release_plan": plan,
            "approval_subject": approval_subject,
            "outcomes": outcomes.iter().map(effect_outcome).collect::<Vec<_>>(),
            "phase_state": phases,
            "pending_intents": pending_intents,
            "synthetic_executor_calls": executor.calls,
            "placement_instructions": completed.then_some(&plan.placement_instructions),
            "next_permitted_actions": if completed { vec!["follow_placement_instructions"] } else { vec!["resume"] },
        }),
    ))
}

fn status(arguments: StatusArgs, state_root: PathBuf) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("status", &arguments.repository)?;
    let identity = journal_identity("status", &prepared, &arguments.train)?;
    let journal = JournalStore::new(&state_root, identity.clone())
        .map_err(|error| map_state_error("status", error))?;
    let approval = ApprovalStore::new(&state_root, identity)
        .map_err(|error| map_approval_error("status", error))?
        .load()
        .map_err(|error| map_approval_error("status", error))?;
    let pinned = journal
        .pinned_declaration()
        .map_err(|error| map_state_error("status", error))?;
    let pending_intents = journal
        .pending_intents()
        .map_err(|error| map_state_error("status", error))?;
    let records = journal
        .read_journal()
        .map_err(|error| map_state_error("status", error))?;
    let train = find_train("status", &prepared.declaration, &arguments.train)?;
    let public_effect_count = planned_public_effect_count(train);
    let completion_count = records
        .iter()
        .filter(|record| matches!(record, JournalRecord::Completion { .. }))
        .count();
    let terminal = journal
        .terminal_state()
        .map_err(|error| map_state_error("status", error))?;
    let declaration_matches_pin = pinned
        .as_ref()
        .map(|binding| binding.digest == prepared.declaration.digest);
    let next_actions = next_actions(
        terminal.is_some(),
        declaration_matches_pin,
        pending_intents.len(),
        public_effect_count,
        completion_count,
        &journal.train_journal_id(),
    );

    Ok(success(
        "status",
        json!({
            "repository": prepared.id,
            "train": arguments.train,
            "journal_id": journal.train_journal_id(),
            "pinned_declaration": pinned,
            "active_declaration_digest": prepared.declaration.digest,
            "declaration_matches_pin": declaration_matches_pin,
            "approval_subject": approval.map(|record| record.subject),
            "terminal_state": terminal,
            "leases": {
                "repository_train": "required_for_mutation; not acquired by status",
                "repository": train.phases.iter().any(|phase| matches!(phase.phase_type.as_str(), "stamp" | "tag"))
                    .then_some("required_for_tree_mutation; not acquired by status"),
            },
            "phase_state": phase_state(&prepared.declaration, &arguments.train, &journal, "status")?,
            "pending_intents": pending_intents,
            "probe_conclusions": pending_probe_conclusions(&journal, "status")?,
            "next_permitted_actions": next_actions,
            "provider_accessed": false,
        }),
    ))
}

fn abandon(arguments: CeremonyArgs, state_root: PathBuf) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("abandon", &arguments.repository)?;
    let identity = journal_identity("abandon", &prepared, &arguments.train)?;
    let journal = JournalStore::new(&state_root, identity)
        .map_err(|error| map_state_error("abandon", error))?;
    let terminal =
        ceremony::abandon(&journal).map_err(|error| map_ceremony_error("abandon", error))?;
    Ok(success(
        "abandon",
        json!({
            "journal_id": journal.train_journal_id(),
            "terminal_state": terminal,
            "evidence_retained": true,
        }),
    ))
}

fn rebind(arguments: RebindArgs, state_root: PathBuf) -> Result<MachineResponse, CliFailure> {
    let prepared = load_repository("rebind", &arguments.repository)?;
    let identity = journal_identity("rebind", &prepared, &arguments.train)?;
    let journal = JournalStore::new(&state_root, identity.clone())
        .map_err(|error| map_state_error("rebind", error))?;
    let preview = ceremony::prepare_rebind(&journal, &prepared.declaration)
        .map_err(|error| map_ceremony_error("rebind", error))?;
    let preview_data = json!({
        "journal_id": preview.train_journal_id,
        "pinned_digest": preview.pinned_digest,
        "replacement_digest": preview.replacement_digest,
        "differences": preview.differences.iter().map(|difference| json!({
            "path": difference.path,
            "pinned": difference.pinned,
            "replacement": difference.replacement,
        })).collect::<Vec<_>>(),
        "diff": preview.render_diff(),
    });

    let Some(confirmation) = arguments.confirm else {
        return Ok(success(
            "rebind",
            json!({
                "preview": preview_data,
                "requires_confirmation": true,
                "next_permitted_actions": ["rerun rebind with --confirm <replacement_digest>"],
            }),
        ));
    };
    if confirmation != preview.replacement_digest.to_string() {
        return Err(CliFailure::refusal_with_context(
            "rebind",
            "rebind_confirmation_mismatch",
            "--confirm must equal the replacement digest shown by the rebind preview",
            json!({"preview": preview_data}),
        ));
    }
    let approvals = ApprovalStore::new(&state_root, identity)
        .map_err(|error| map_approval_error("rebind", error))?;
    let outcome =
        ceremony::confirm_rebind(&journal, &approvals, preview, RebindConfirmation::Confirmed)
            .map_err(|error| map_ceremony_error("rebind", error))?;
    Ok(success(
        "rebind",
        json!({
            "preview": preview_data,
            "pinned_declaration": outcome.binding,
            "invalidated_approval": outcome.invalidated_approval,
            "approval_reconstruction_required": outcome.approval_reconstruction_required,
        }),
    ))
}

fn load_repository(
    command: &str,
    arguments: &RepositoryArgs,
) -> Result<PreparedRepository, CliFailure> {
    let requested_path = arguments
        .repo
        .clone()
        .map(Ok)
        .unwrap_or_else(env::current_dir)
        .map_err(|error| {
            CliFailure::internal(command, "current_directory_unavailable", error.to_string())
        })?;
    let path = fs::canonicalize(&requested_path).map_err(|error| {
        CliFailure::refusal(
            command,
            "repository_not_found",
            format!(
                "cannot resolve repository {}: {error}",
                requested_path.display()
            ),
        )
    })?;
    if !path.is_dir() {
        return Err(CliFailure::refusal(
            command,
            "repository_not_directory",
            format!("repository {} is not a directory", path.display()),
        ));
    }
    let declaration = declaration::load(path.join(".cortexkit/release.jsonc"))
        .map_err(|error| map_declaration_error(command, error))?;
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    Ok(PreparedRepository {
        id: RepositoryId::new(format!("repo-{:x}", digest)),
        declaration,
    })
}

fn find_train<'a>(
    command: &str,
    declaration: &'a ParsedDeclaration,
    train_name: &str,
) -> Result<&'a TrainDeclaration, CliFailure> {
    declaration
        .declaration
        .trains
        .iter()
        .find(|train| train.id == train_name)
        .ok_or_else(|| {
            CliFailure::refusal(
                command,
                "plan_unknown_train",
                format!("release declaration has no train named `{train_name}`"),
            )
        })
}

fn journal_identity(
    command: &str,
    prepared: &PreparedRepository,
    train_name: &str,
) -> Result<TrainJournalIdentity, CliFailure> {
    let train = find_train(command, &prepared.declaration, train_name)?;
    TrainJournalIdentity::new(
        prepared.id.clone(),
        train.train_id(),
        train.intended_commit.clone(),
    )
    .map_err(|error| map_state_error(command, error))
}

fn journal_id_for(train: &TrainDeclaration) -> String {
    format!("{}-{}", train.id, train.intended_commit)
}

fn build_plan(
    command: &str,
    prepared: &PreparedRepository,
    train_name: &str,
    artifact_arguments: &[String],
) -> Result<ReleasePlan, CliFailure> {
    let train = find_train(command, &prepared.declaration, train_name)?;
    let artifacts = finalized_artifacts(command, train, artifact_arguments)?;
    plan::build_dry_run_plan(
        prepared.id.clone(),
        &prepared.declaration,
        train_name,
        &artifacts,
    )
    .map_err(|error| map_plan_error(command, error))
}

fn finalized_artifacts(
    command: &str,
    train: &TrainDeclaration,
    artifact_arguments: &[String],
) -> Result<Vec<FinalizedArtifact>, CliFailure> {
    let channels = train
        .artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .identity_channel
                .as_ref()
                .map(|channel| (artifact.id.as_str(), channel.as_str()))
        })
        .collect::<HashMap<_, _>>();
    artifact_arguments
        .iter()
        .map(|argument| {
            let (artifact, path) = argument.split_once('=').ok_or_else(|| {
                CliFailure::refusal(
                    command,
                    "invalid_artifact_argument",
                    format!("artifact `{argument}` must use <artifact-id>=<path>"),
                )
            })?;
            if artifact.trim().is_empty() || path.trim().is_empty() {
                return Err(CliFailure::refusal(
                    command,
                    "invalid_artifact_argument",
                    format!("artifact `{argument}` must include both an id and a path"),
                ));
            }
            let bytes = fs::read(path).map_err(|error| {
                CliFailure::refusal(
                    command,
                    "artifact_unreadable",
                    format!("cannot read artifact {path}: {error}"),
                )
            })?;
            let identity = artifact_identity(train, channels.get(artifact).copied(), &bytes);
            Ok(FinalizedArtifact {
                artifact: ArtifactId::new(artifact),
                identity,
                bytes,
            })
        })
        .collect()
}

fn artifact_identity(train: &TrainDeclaration, channel: Option<&str>, bytes: &[u8]) -> String {
    match channel {
        Some("asset_sha256") => format!("{:x}", Sha256::digest(bytes)),
        Some("embedded_build_sha") | Some("tag_at_commit") => train.intended_commit.clone(),
        Some("registry_version") | Some("gh_release") => {
            train.tag.clone().unwrap_or_else(|| train.train_key())
        }
        _ => format!("{:x}", Sha256::digest(bytes)),
    }
}

fn phase_state(
    declaration: &ParsedDeclaration,
    train_name: &str,
    journal: &JournalStore,
    command: &str,
) -> Result<Vec<Value>, CliFailure> {
    let train = find_train(command, declaration, train_name)?;
    let completed = journal
        .read_journal()
        .map_err(|error| map_state_error(command, error))?
        .into_iter()
        .filter_map(|record| match record {
            JournalRecord::Completion { intent, .. } => Some(intent.phase.to_string()),
            JournalRecord::DeclarationPinned { .. }
            | JournalRecord::DeclarationRebound { .. }
            | JournalRecord::Terminalized { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let pending = journal
        .pending_intents()
        .map_err(|error| map_state_error(command, error))?
        .into_iter()
        .map(|intent| intent.phase.to_string())
        .collect::<BTreeSet<_>>();
    Ok(train
        .phases
        .iter()
        .map(|phase| {
            let state = match (completed.contains(&phase.id), pending.contains(&phase.id)) {
                (true, true) => "partially_completed",
                (true, false) => "completed",
                (false, true) => "awaiting_probe",
                (false, false) => "not_started",
            };
            json!({"instance": phase.id, "type": phase.phase_type, "state": state})
        })
        .collect())
}

fn pending_probe_conclusions(
    journal: &JournalStore,
    command: &str,
) -> Result<Vec<Value>, CliFailure> {
    journal
        .pending_intents()
        .map_err(|error| map_state_error(command, error))
        .map(|intents| {
            intents
                .into_iter()
                .map(|intent| {
                    json!({
                        "phase": intent.phase,
                        "artifact": intent.artifact,
                        "operation": intent.operation,
                        "conclusion": {
                            "kind": "not_probed",
                            "reason": "status never invokes a provider or acquires release credentials",
                        },
                    })
                })
                .collect()
        })
}

fn planned_public_effect_count(train: &TrainDeclaration) -> usize {
    train
        .phases
        .iter()
        .map(|phase| match phase.phase_type.as_str() {
            "tag" => 1,
            "publish" | "assets" => train.artifacts.len(),
            _ => 0,
        })
        .sum()
}

fn next_actions(
    terminal: bool,
    declaration_matches_pin: Option<bool>,
    pending_count: usize,
    public_effect_count: usize,
    completion_count: usize,
    journal_id: &str,
) -> Vec<String> {
    if terminal {
        return Vec::new();
    }
    if declaration_matches_pin == Some(false) {
        return vec![
            format!("abandon {journal_id}"),
            format!("rebind {journal_id}"),
        ];
    }
    if pending_count > 0 {
        return vec!["resume".to_owned()];
    }
    if public_effect_count > 0 && completion_count >= public_effect_count {
        return vec!["follow_placement_instructions".to_owned()];
    }
    vec!["execute".to_owned()]
}

fn effect_outcome(outcome: &EffectOutcome) -> &'static str {
    match outcome {
        EffectOutcome::Executed(_) => "executed",
        EffectOutcome::Reconciled(_) => "reconciled",
        EffectOutcome::AwaitingProbe => "awaiting_probe",
    }
}

struct NoopPhaseRunner;

impl PhaseRunner for NoopPhaseRunner {
    fn run(&mut self, _phase: &crate::plan::PlannedPhase) -> Result<(), SeamError> {
        Ok(())
    }
}

struct SyntheticApprovalGate {
    confirmed: bool,
}

impl FirstPublicTriggerGate for SyntheticApprovalGate {
    fn confirm(&mut self, subject: &ApprovalSubject) -> Result<ApprovalToken, SeamError> {
        if !self.confirmed {
            return Err(SeamError::new(
                "operator confirmation is required before the first public trigger",
            ));
        }
        Ok(ApprovalToken::new(format!(
            "synthetic-confirmation-{}",
            subject.declaration_digest
        )))
    }
}

#[derive(Deserialize, Serialize)]
struct SyntheticEffect {
    operation: String,
    identity: String,
}

/// A durable fake external service used only by the synthetic end-to-end train.
struct SyntheticProvider {
    effects_root: PathBuf,
    expected_identities: HashMap<String, String>,
    interrupt_after_effect: bool,
    calls: usize,
}

impl SyntheticProvider {
    fn new(
        state_root: &Path,
        plan: &ReleasePlan,
        interrupt_after_effect: bool,
    ) -> Result<Self, String> {
        let mut expected_identities = plan
            .artifacts
            .iter()
            .map(|artifact| (artifact.artifact.to_string(), artifact.identity.clone()))
            .collect::<HashMap<_, _>>();
        expected_identities.insert("tag".to_owned(), plan.intended_commit.to_string());
        let effects_root = state_root
            .join(plan.repository.as_str())
            .join("synthetic-provider-effects");
        fs::create_dir_all(&effects_root).map_err(|error| error.to_string())?;
        Ok(Self {
            effects_root,
            expected_identities,
            interrupt_after_effect,
            calls: 0,
        })
    }

    fn effect_path(&self, request: &EffectRequest) -> PathBuf {
        let digest = Sha256::digest(request.operation.as_str().as_bytes());
        self.effects_root.join(format!("{digest:x}.json"))
    }

    fn expected_identity(&self, request: &EffectRequest) -> Result<String, SeamError> {
        self.expected_identities
            .get(request.artifact.as_str())
            .cloned()
            .ok_or_else(|| {
                SeamError::new(format!(
                    "no synthetic identity for artifact `{}`",
                    request.artifact
                ))
            })
    }

    fn read_effect(&self, request: &EffectRequest) -> Result<Option<SyntheticEffect>, SeamError> {
        let path = self.effect_path(request);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                SeamError::new(format!(
                    "cannot read synthetic effect {}: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SeamError::new(format!(
                "cannot read synthetic effect {}: {error}",
                path.display()
            ))),
        }
    }
}

impl CompletionProbe for SyntheticProvider {
    fn probe(&mut self, request: &EffectRequest) -> Result<ProbeResult, SeamError> {
        let expected_identity = self.expected_identity(request)?;
        Ok(match self.read_effect(request)? {
            Some(effect) => ProbeResult::Present(ProbeEvidence {
                reference: format!("synthetic:{}", effect.operation),
                identity: effect.identity,
            }),
            None => ProbeResult::Absent(ProbeEvidence {
                reference: format!("synthetic:absent:{}", request.operation),
                identity: expected_identity,
            }),
        })
    }
}

impl IrreversibleExecutor for SyntheticProvider {
    fn execute(&mut self, request: &EffectRequest) -> Result<ProbeEvidence, SeamError> {
        self.calls += 1;
        let expected_identity = self.expected_identity(request)?;
        if let Some(effect) = self.read_effect(request)? {
            return Ok(ProbeEvidence {
                reference: format!("synthetic:{}", effect.operation),
                identity: effect.identity,
            });
        }

        let path = self.effect_path(request);
        let effect = SyntheticEffect {
            operation: request.operation.to_string(),
            identity: expected_identity.clone(),
        };
        let encoded = serde_json::to_vec(&effect)
            .map_err(|error| SeamError::new(format!("cannot encode synthetic effect: {error}")))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                SeamError::new(format!(
                    "cannot create synthetic effect {}: {error}",
                    path.display()
                ))
            })?;
        file.write_all(&encoded).map_err(|error| {
            SeamError::new(format!(
                "cannot write synthetic effect {}: {error}",
                path.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            SeamError::new(format!(
                "cannot synchronize synthetic effect {}: {error}",
                path.display()
            ))
        })?;

        if self.interrupt_after_effect {
            return Err(SeamError::new(
                "synthetic interruption after external effect and before completion append",
            ));
        }
        Ok(ProbeEvidence {
            reference: format!("synthetic:{}", request.operation),
            identity: expected_identity,
        })
    }
}

fn map_declaration_error(command: &str, error: DeclarationError) -> CliFailure {
    let code = match error.code {
        DeclarationRefusalCode::Parse => "declaration_parse",
        DeclarationRefusalCode::UnsupportedFormatVersion => {
            "unsupported_declaration_format_version"
        }
        DeclarationRefusalCode::DuplicateTrainId => "duplicate_train_id",
        DeclarationRefusalCode::DuplicatePhaseId => "duplicate_phase_id",
        DeclarationRefusalCode::DuplicateArtifactId => "duplicate_artifact_id",
        DeclarationRefusalCode::UnknownPhaseType => "unknown_phase_type",
        DeclarationRefusalCode::InvalidPhaseParameters => "invalid_phase_parameters",
        DeclarationRefusalCode::MissingArtifactIdentityChannel => {
            "missing_artifact_identity_channel"
        }
        DeclarationRefusalCode::InvalidArtifactIdentityChannel => {
            "invalid_artifact_identity_channel"
        }
        DeclarationRefusalCode::InvalidSigningProfile => "invalid_signing_profile",
        DeclarationRefusalCode::UnsafeOperatorGate => "unsafe_operator_gate",
        DeclarationRefusalCode::MissingFirstPublicTrigger => "missing_first_public_trigger",
        DeclarationRefusalCode::UnsafePhaseOrdering => "unsafe_phase_ordering",
        DeclarationRefusalCode::InvalidNoTagTrain => "invalid_no_tag_train",
    };
    CliFailure::refusal_with_context(
        command,
        code,
        error.message,
        json!({"location": error.location.map(|location| json!({"line": location.line, "column": location.column}))}),
    )
}

fn map_plan_error(command: &str, error: PlanError) -> CliFailure {
    let code = match &error {
        PlanError::UnknownTrain(_) => "plan_unknown_train",
        PlanError::MissingArtifact { .. } => "plan_missing_artifact",
        PlanError::UnexpectedArtifact { .. } => "plan_unexpected_artifact",
        PlanError::DuplicateArtifact { .. } => "plan_duplicate_artifact",
        PlanError::EmptyArtifactIdentity { .. } => "plan_empty_artifact_identity",
    };
    CliFailure::refusal(command, code, error.to_string())
}

fn map_state_error(command: &str, error: StateError) -> CliFailure {
    let code = match &error {
        StateError::HomeDirectoryUnavailable => "home_directory_unavailable",
        StateError::UnsafeIdentity(_) => "unsafe_journal_identity",
        StateError::Io { .. } => "journal_io_failure",
        StateError::UnsupportedFormatVersion { .. } => "unsupported_journal_format_version",
        StateError::CorruptRecord { .. } => "journal_corruption",
        StateError::TornTail { .. } => "journal_torn_tail",
        StateError::IntentDoesNotMatchTrain => "intent_does_not_match_train",
        StateError::DeclarationNotPinned => "declaration_not_pinned",
        StateError::DeclarationDigestMismatch { .. } => "declaration_digest_mismatch",
        StateError::DeclarationBindingChanged { .. } => "declaration_binding_changed",
        StateError::DeclarationDigestUnchanged { .. } => "declaration_digest_unchanged",
        StateError::TrainTerminal { .. } => "train_terminal",
        StateError::Executor(_) => "executor_failure",
    };
    CliFailure::refusal(command, code, error.to_string())
}

fn map_approval_error(command: &str, error: ApprovalError) -> CliFailure {
    let code = match &error {
        ApprovalError::NoPublicTrigger { .. } => "approval_no_public_trigger",
        ApprovalError::InconsistentPublicTrigger { .. } => "approval_inconsistent_public_trigger",
        ApprovalError::SubjectDoesNotMatchStore => "approval_store_subject_mismatch",
        ApprovalError::NoCurrentApproval => "approval_required",
        ApprovalError::SubjectMismatch => "approval_mismatch",
        ApprovalError::UnsupportedFormatVersion { .. } => "unsupported_approval_format_version",
        ApprovalError::CorruptRecord { .. } => "approval_corruption",
        ApprovalError::Io { .. } => "approval_io_failure",
        ApprovalError::UnsupportedDurability { .. } => "approval_durability_unavailable",
    };
    CliFailure::refusal(command, code, error.to_string())
}

fn map_lease_error(command: &str, error: LeaseError) -> CliFailure {
    let code = match &error {
        LeaseError::Conflict { .. } => "lease_conflict",
        LeaseError::UnsafeIdentity(_) => "unsafe_lease_identity",
        LeaseError::UnsupportedLocking { .. } => "lease_locking_unavailable",
        LeaseError::UnsupportedDurability { .. } => "lease_durability_unavailable",
        LeaseError::Io { .. } => "lease_io_failure",
        LeaseError::CorruptHolder { .. } => "lease_holder_corruption",
    };
    CliFailure::refusal(command, code, error.to_string())
}

fn map_ceremony_error(command: &str, error: CeremonyError) -> CliFailure {
    match error {
        CeremonyError::Refusal(refusal) => CliFailure::refusal_with_context(
            command,
            refusal.code.as_str(),
            refusal.message,
            json!({"journal_id": refusal.train_journal_id}),
        ),
        CeremonyError::State(error) => map_state_error(command, error),
        CeremonyError::Approval(error) => map_approval_error(command, error),
    }
}

fn map_orchestration_error(command: &str, error: OrchestrationError) -> CliFailure {
    match error {
        OrchestrationError::Refusal {
            code,
            phase,
            message,
        } => {
            let code = match code {
                OrchestrationRefusalCode::UnknownPhase => "unknown_phase",
                OrchestrationRefusalCode::UnsafeOrdering => "unsafe_phase_ordering",
                OrchestrationRefusalCode::AttemptedIntentAbsent => "attempted_intent_absent",
                OrchestrationRefusalCode::ContradictoryEvidence => {
                    "contradictory_provider_evidence"
                }
                OrchestrationRefusalCode::MissingPublicEffect => "missing_public_effect",
                OrchestrationRefusalCode::DeclarationDigestMismatch => {
                    "declaration_digest_mismatch"
                }
            };
            CliFailure::refusal_with_context(command, code, message, json!({"phase": phase}))
        }
        OrchestrationError::Lease(error) => map_lease_error(command, error),
        OrchestrationError::State(error) => map_state_error(command, error),
        OrchestrationError::Approval(error) => map_approval_error(command, error),
        OrchestrationError::Seam(error) => map_seam_error(command, error),
    }
}

fn map_seam_error(command: &str, error: SeamError) -> CliFailure {
    if error
        .message()
        .contains("synthetic interruption after external effect")
    {
        return CliFailure::internal(command, "synthetic_interruption", error.message());
    }
    CliFailure::internal(command, "provider_or_phase_failure", error.message())
}

fn map_synthetic_error(command: &str, message: String) -> CliFailure {
    CliFailure::internal(command, "synthetic_provider_state_failure", message)
}

#[cfg(test)]
#[path = "../../tests/e2e/mod.rs"]
mod e2e;
