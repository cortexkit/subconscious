//! Declared local command gates executed against a release repository.

use crate::{
    declaration::PhaseDeclaration,
    orchestrator::{PhaseExecutionError, PhaseRunner},
    phases::precheck::PrecheckRunner,
    plan::PlannedPhase,
    state::{JournalRecord, JournalStore},
    ProbeEvidence, SeamError,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub const GATES_LOCAL: &str = "gates_local";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatesLocalConfig {
    command: String,
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Script-level retries inside a leg answer "is this leg's verdict trustworthy" (per-file, one process tree); machine-level `retry_budget` answers "does this leg get another attempt" (per-leg, fresh process). Different questions — composing them is correct layering, not double-counting; the journal records leg attempts while script-internal retries live in the captured output artifact.
    #[serde(default)]
    retry_budget: u32,
    /// The declared taxonomy label is validated and recorded for each attempt. V1 executes legs serially in declaration order and does not schedule on `load_class`.
    load_class: String,
}

impl GatesLocalConfig {
    fn validate(&self) -> Result<(), String> {
        require_nonempty(&self.command, "command")?;
        require_nonempty(&self.load_class, "load_class")?;
        for name in self.env.keys() {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(format!("env contains invalid variable name `{name}`"));
            }
        }
        if let Some(cwd) = &self.cwd {
            if cwd.as_os_str().is_empty() {
                return Err("cwd must not be empty when supplied".to_owned());
            }
            if cwd.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            }) {
                return Err("cwd must be relative to the repository root".to_owned());
            }
        }
        Ok(())
    }
}

/// Validates the strict parameter schema for a declared local command gate.
pub fn validate_parameters(phase: &PhaseDeclaration) -> Result<(), String> {
    if phase.phase_type != GATES_LOCAL {
        return Ok(());
    }
    serde_json::from_value::<GatesLocalConfig>(phase.params.clone())
        .map_err(|error| error.to_string())?
        .validate()
}

/// Executes declared local command gates and delegates declared prechecks to their existing runner.
pub struct CommandPhaseRunner<'a> {
    repository: &'a Path,
    journal: &'a JournalStore,
    prechecks: PrecheckRunner<'a>,
}

impl<'a> CommandPhaseRunner<'a> {
    pub fn new(repository: &'a Path, journal: &'a JournalStore) -> Self {
        Self {
            repository,
            journal,
            prechecks: PrecheckRunner::new(repository, journal),
        }
    }

    fn run_gates_local(
        &self,
        phase: &PlannedPhase,
    ) -> Result<Vec<ProbeEvidence>, PhaseExecutionError> {
        let config = parse_config(phase)?;
        let cwd = config.cwd.as_deref().map_or_else(
            || self.repository.to_path_buf(),
            |cwd| self.repository.join(cwd),
        );
        let mut evidence = Vec::new();
        let mut failures = Vec::new();

        for retries_used in 0..=u64::from(config.retry_budget) {
            let attempt = retries_used + 1;
            let output_path = self
                .journal
                .evidence_dir()
                .join(format!("{}-attempt-{attempt}.log", phase.instance));
            let output = Command::new(&config.command)
                .args(&config.args)
                .current_dir(&cwd)
                .envs(&config.env)
                .output();

            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    write_output_artifact(&output_path, &[], error.to_string().as_bytes())?;
                    self.append_attempt(
                        phase,
                        attempt,
                        None,
                        output_path.clone(),
                        &config.load_class,
                    )?;
                    return Err(SeamError::new(format!(
                        "gates_local phase `{}` could not spawn `{}` on attempt {attempt}: {error}; output: {}",
                        phase.instance,
                        config.command,
                        output_path.display()
                    ))
                    .into());
                }
            };
            write_output_artifact(&output_path, &output.stdout, &output.stderr)?;
            let exit_code = output.status.code();
            self.append_attempt(
                phase,
                attempt,
                exit_code,
                output_path.clone(),
                &config.load_class,
            )?;
            evidence.push(ProbeEvidence {
                reference: output_path.to_string_lossy().into_owned(),
                identity: format!(
                    "gates_local load_class={} attempt={attempt}",
                    config.load_class
                ),
            });

            if output.status.success() {
                return Ok(evidence);
            }

            failures.push((attempt, exit_code, output_path));
            if retries_used < u64::from(config.retry_budget) {
                continue;
            }
            let trail = failures
                .iter()
                .map(|(attempt, exit_code, path)| {
                    format!(
                        "attempt {attempt} exited {} ({})",
                        exit_code.map_or_else(
                            || "without an exit code".to_owned(),
                            |code| code.to_string()
                        ),
                        path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SeamError::new(format!(
                "gates_local phase `{}` exhausted retry_budget {}: {trail}",
                phase.instance, config.retry_budget
            ))
            .into());
        }

        unreachable!("the inclusive retry loop always returns on success or exhaustion")
    }

    fn append_attempt(
        &self,
        phase: &PlannedPhase,
        attempt: u64,
        exit_code: Option<i32>,
        output_path: PathBuf,
        load_class: &str,
    ) -> Result<(), PhaseExecutionError> {
        self.journal
            .append_journal(JournalRecord::LocalCommandAttempt {
                phase: phase.instance.clone(),
                attempt,
                exit_code,
                output_path,
                load_class: load_class.to_owned(),
            })
            .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))
    }
}

impl PhaseRunner for CommandPhaseRunner<'_> {
    fn run(&mut self, phase: &PlannedPhase) -> Result<Vec<ProbeEvidence>, PhaseExecutionError> {
        match phase.phase_type.as_str() {
            GATES_LOCAL => self.run_gates_local(phase),
            _ => self.prechecks.run(phase),
        }
    }
}

fn parse_config(phase: &PlannedPhase) -> Result<GatesLocalConfig, PhaseExecutionError> {
    let config: GatesLocalConfig =
        serde_json::from_value(phase.params.clone()).map_err(|error| {
            PhaseExecutionError::Seam(SeamError::new(format!(
                "validated parameters for phase `{}` no longer decode: {error}",
                phase.instance
            )))
        })?;
    config
        .validate()
        .map_err(|message| PhaseExecutionError::Seam(SeamError::new(message)))?;
    Ok(config)
}

fn require_nonempty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn write_output_artifact(
    output_path: &Path,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(), PhaseExecutionError> {
    let evidence_dir = output_path
        .parent()
        .expect("an evidence artifact path always has a parent");
    fs::create_dir_all(evidence_dir)
        .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
    let mut artifact = fs::File::create(output_path)
        .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
    artifact
        .write_all(stdout)
        .and_then(|()| artifact.write_all(stderr))
        .and_then(|()| artifact.sync_all())
        .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))
}
