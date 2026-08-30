//! Declared local prechecks that run before the first irreversible release phase.

use crate::{
    orchestrator::{PhaseExecutionError, PhaseRunner, PrecheckRefusalCode},
    plan::PlannedPhase,
    state::{JournalRecord, JournalStore},
    SeamError,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    env, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
};

pub const FORMAT_DIRTY: &str = "precheck-format-dirty";
pub const STALE_RESIDUE: &str = "precheck-stale-residue";
pub const SIBLING_DRIFT: &str = "precheck-sibling-drift";
pub const CONTEXT_FITNESS: &str = "precheck-context-fitness";
pub const TOOL_PINNING: &str = "precheck-tool-pinning";
pub const RESIDUE_SWEEP: &str = "precheck-residue-sweep";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatDirtyConfig {
    tool: String,
    command: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StaleResidueConfig {
    residue_globs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiblingDriftConfig {
    siblings: Vec<SiblingRequirement>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiblingRequirement {
    name: String,
    path: PathBuf,
    expected_ref: String,
    #[serde(default)]
    clean: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextFitnessConfig {
    requirements: Vec<ContextRequirement>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ContextRequirement {
    Tool {
        name: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        minimum_version: Option<String>,
    },
    Env {
        name: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolPinningConfig {
    tools: Vec<ToolPin>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolPin {
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    exact_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidueSweepConfig {
    #[serde(default)]
    clearable_globs: Vec<String>,
    #[serde(default)]
    pid_files: Vec<NamedPath>,
    #[serde(default)]
    ports: Vec<NamedPort>,
    #[serde(default)]
    foreign_locks: Vec<NamedPath>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedPath {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedPort {
    name: String,
    port: u16,
}

/// Validates the strict parameter schema for a declared precheck phase.
pub fn validate_parameters(phase: &crate::declaration::PhaseDeclaration) -> Result<(), String> {
    let value = phase.params.clone();
    let result = match phase.phase_type.as_str() {
        FORMAT_DIRTY => serde_json::from_value::<FormatDirtyConfig>(value).map(|config| {
            require_nonempty(&config.tool, "tool")?;
            require_command(&config.command)
        }),
        STALE_RESIDUE => serde_json::from_value::<StaleResidueConfig>(value)
            .map(|config| require_strings(&config.residue_globs, "residue_globs")),
        SIBLING_DRIFT => serde_json::from_value::<SiblingDriftConfig>(value).map(|config| {
            if config.siblings.is_empty() {
                return Err("siblings must not be empty".to_owned());
            }
            for sibling in config.siblings {
                require_nonempty(&sibling.name, "sibling name")?;
                require_nonempty(&sibling.expected_ref, "expected_ref")?;
                if sibling.path.as_os_str().is_empty() {
                    return Err("sibling path must not be empty".to_owned());
                }
            }
            Ok(())
        }),
        CONTEXT_FITNESS => serde_json::from_value::<ContextFitnessConfig>(value).map(|config| {
            if config.requirements.is_empty() {
                return Err("requirements must not be empty".to_owned());
            }
            Ok(())
        }),
        TOOL_PINNING => serde_json::from_value::<ToolPinningConfig>(value).map(|config| {
            if config.tools.is_empty() {
                return Err("tools must not be empty".to_owned());
            }
            for tool in config.tools {
                require_nonempty(&tool.name, "tool name")?;
            }
            Ok(())
        }),
        RESIDUE_SWEEP => serde_json::from_value::<ResidueSweepConfig>(value).map(|config| {
            if config.clearable_globs.is_empty()
                && config.pid_files.is_empty()
                && config.ports.is_empty()
                && config.foreign_locks.is_empty()
            {
                return Err("at least one residue check must be declared".to_owned());
            }
            Ok(())
        }),
        _ => return Ok(()),
    };
    result
        .map_err(|error| error.to_string())
        .and_then(|result| result)
}

fn require_nonempty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn require_strings(values: &[String], field: &str) -> Result<(), String> {
    if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
        Err(format!("{field} must contain non-empty values"))
    } else {
        Ok(())
    }
}

fn require_command(command: &[String]) -> Result<(), String> {
    require_strings(command, "command")
}

/// Executes prechecks against one repository and its live train journal.
pub struct PrecheckRunner<'a> {
    repository: &'a Path,
    journal: &'a JournalStore,
}

impl<'a> PrecheckRunner<'a> {
    pub fn new(repository: &'a Path, journal: &'a JournalStore) -> Self {
        Self {
            repository,
            journal,
        }
    }

    fn own_run_paths(&self) -> Result<HashSet<PathBuf>, PhaseExecutionError> {
        let records = self
            .journal
            .read_journal()
            .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
        if records
            .iter()
            .any(|record| matches!(record, JournalRecord::Terminalized { .. }))
        {
            return Ok(HashSet::new());
        }
        Ok(records
            .into_iter()
            .filter_map(|record| match record {
                JournalRecord::WorkingTreeMutation { paths, .. } => Some(paths),
                _ => None,
            })
            .flatten()
            .map(|path| self.absolute(&path))
            .collect())
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.repository.join(path)
        }
    }

    fn predecessor_note(&self, paths: &[PathBuf]) -> Result<String, PhaseExecutionError> {
        let relative = paths
            .iter()
            .map(|path| {
                path.strip_prefix(self.repository)
                    .unwrap_or(path)
                    .to_path_buf()
            })
            .collect::<Vec<_>>();
        let origins = self
            .journal
            .predecessor_mutation_origins(&relative)
            .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
        if origins.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!("; predecessor train(s): {}", origins.join(", ")))
        }
    }

    fn refuse(
        phase: &PlannedPhase,
        code: PrecheckRefusalCode,
        message: impl Into<String>,
    ) -> PhaseExecutionError {
        PhaseExecutionError::Refusal {
            code,
            phase: phase.instance.clone(),
            message: message.into(),
        }
    }

    fn run_format_dirty(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: FormatDirtyConfig = parse_config(phase)?;
        let output = run_declared_command(self.repository, &config.command).map_err(|error| {
            Self::refuse(
                phase,
                PrecheckRefusalCode::PrecheckDirty,
                format!("tool `{}` could not run: {error}", config.tool),
            )
        })?;
        let dirty = git_dirty_paths(self.repository)?;
        let own = self.own_run_paths()?;
        let unexpected = dirty
            .into_iter()
            .filter(|path| !own.contains(path))
            .collect::<Vec<_>>();
        if !output.status.success() || !unexpected.is_empty() {
            let predecessor = self.predecessor_note(&unexpected)?;
            let paths = display_paths(self.repository, &unexpected);
            return Err(Self::refuse(
                phase,
                PrecheckRefusalCode::PrecheckDirty,
                format!(
                    "tool `{}` reported dirty files [{}] (exit {})",
                    config.tool,
                    paths.join(", "),
                    output.status,
                ) + &predecessor,
            ));
        }
        Ok(())
    }

    fn run_stale_residue(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: StaleResidueConfig = parse_config(phase)?;
        let own = self.own_run_paths()?;
        let paths = matching_paths(self.repository, &config.residue_globs)?
            .into_iter()
            .filter(|path| !own.contains(path))
            .collect::<Vec<_>>();
        if paths.is_empty() {
            Ok(())
        } else {
            let predecessor = self.predecessor_note(&paths)?;
            Err(Self::refuse(
                phase,
                PrecheckRefusalCode::StaleRunResidue,
                format!(
                    "stale release residue [{}]; no matching live mutation in train {}",
                    display_paths(self.repository, &paths).join(", "),
                    self.journal.train_journal_id()
                ) + &predecessor,
            ))
        }
    }

    fn run_sibling_drift(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: SiblingDriftConfig = parse_config(phase)?;
        for sibling in config.siblings {
            let path = self.absolute(&sibling.path);
            let observed = git_output(&path, &["rev-parse", "HEAD"])?;
            if observed != sibling.expected_ref {
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::EnvDrift,
                    format!(
                        "sibling `{}` expected {}, observed {} at {}",
                        sibling.name,
                        sibling.expected_ref,
                        observed,
                        path.display()
                    ),
                ));
            }
            if sibling.clean && !git_dirty_paths(&path)?.is_empty() {
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::EnvDrift,
                    format!(
                        "sibling `{}` is not clean at {}",
                        sibling.name,
                        path.display()
                    ),
                ));
            }
        }
        Ok(())
    }

    fn run_context_fitness(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: ContextFitnessConfig = parse_config(phase)?;
        for requirement in config.requirements {
            match requirement {
                ContextRequirement::Env { name } => {
                    if env::var_os(&name).is_none() {
                        return Err(Self::refuse(
                            phase,
                            PrecheckRefusalCode::ContextUnfit,
                            format!("required environment variable `{name}` is absent"),
                        ));
                    }
                }
                ContextRequirement::Tool {
                    name,
                    command,
                    minimum_version,
                } => {
                    let executable = command.as_deref().unwrap_or(&name);
                    let observed = probe_version(executable).map_err(|error| {
                        Self::refuse(
                            phase,
                            PrecheckRefusalCode::ContextUnfit,
                            format!("required tool `{name}` is unavailable: {error}"),
                        )
                    })?;
                    if minimum_version
                        .as_deref()
                        .is_some_and(|minimum| version_less_than(&observed, minimum))
                    {
                        return Err(Self::refuse(
                            phase,
                            PrecheckRefusalCode::ContextUnfit,
                            format!(
                                "tool `{name}` requires at least {}, observed {}",
                                minimum_version.expect("checked as some"),
                                observed
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn run_tool_pinning(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: ToolPinningConfig = parse_config(phase)?;
        for tool in config.tools {
            let Some(expected) = tool.exact_version else {
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::ToolUnpinned,
                    format!("tool `{}` has no exact declared pin", tool.name),
                ));
            };
            let executable = tool.command.as_deref().unwrap_or(&tool.name);
            let observed = probe_version(executable).map_err(|error| {
                Self::refuse(
                    phase,
                    PrecheckRefusalCode::ToolMismatch,
                    format!(
                        "tool `{}` expected {}, observed unavailable ({error})",
                        tool.name, expected
                    ),
                )
            })?;
            if normalize_version(&observed) != normalize_version(&expected) {
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::ToolMismatch,
                    format!(
                        "tool `{}` expected {}, observed {}",
                        tool.name, expected, observed
                    ),
                ));
            }
        }
        Ok(())
    }

    fn run_residue_sweep(&self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        let config: ResidueSweepConfig = parse_config(phase)?;
        let own = self.own_run_paths()?;
        for lock in &config.foreign_locks {
            let path = self.absolute(&lock.path);
            if path.exists() && !own.contains(&path) {
                let predecessor = self.predecessor_note(std::slice::from_ref(&path))?;
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::ResiduePresent,
                    format!("foreign lock `{}` remains at {}", lock.name, path.display())
                        + &predecessor,
                ));
            }
        }
        for process in &config.pid_files {
            let path = self.absolute(&process.path);
            if path.exists() && !own.contains(&path) {
                let pid = fs::read_to_string(&path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if pid.is_some_and(process_alive) {
                    return Err(Self::refuse(
                        phase,
                        PrecheckRefusalCode::ResiduePresent,
                        format!(
                            "live process `{}` remains with pid {}",
                            process.name,
                            pid.unwrap()
                        ),
                    ));
                }
            }
        }
        for port in &config.ports {
            let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port.port);
            if TcpListener::bind(address).is_err() {
                return Err(Self::refuse(
                    phase,
                    PrecheckRefusalCode::ResiduePresent,
                    format!("port residue `{}` is holding {}", port.name, port.port),
                ));
            }
        }

        let swept = matching_paths(self.repository, &config.clearable_globs)?
            .into_iter()
            .filter(|path| !own.contains(path))
            .collect::<Vec<_>>();
        for path in &swept {
            if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
            .map_err(|error| {
                Self::refuse(
                    phase,
                    PrecheckRefusalCode::ResiduePresent,
                    format!(
                        "clearable residue {} could not be swept: {error}",
                        path.display()
                    ),
                )
            })?;
        }
        if !swept.is_empty() {
            self.journal
                .append_journal(JournalRecord::ResidueSwept {
                    phase: phase.instance.clone(),
                    paths: display_paths(self.repository, &swept),
                })
                .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
        }
        Ok(())
    }
}

impl PhaseRunner for PrecheckRunner<'_> {
    fn run(&mut self, phase: &PlannedPhase) -> Result<(), PhaseExecutionError> {
        match phase.phase_type.as_str() {
            FORMAT_DIRTY => self.run_format_dirty(phase),
            STALE_RESIDUE => self.run_stale_residue(phase),
            SIBLING_DRIFT => self.run_sibling_drift(phase),
            CONTEXT_FITNESS => self.run_context_fitness(phase),
            TOOL_PINNING => self.run_tool_pinning(phase),
            RESIDUE_SWEEP => self.run_residue_sweep(phase),
            _ => Ok(()),
        }
    }
}

fn parse_config<T: for<'de> Deserialize<'de>>(
    phase: &PlannedPhase,
) -> Result<T, PhaseExecutionError> {
    serde_json::from_value(phase.params.clone()).map_err(|error| {
        PhaseExecutionError::Seam(SeamError::new(format!(
            "validated parameters for phase `{}` no longer decode: {error}",
            phase.instance
        )))
    })
}

fn run_declared_command(
    repository: &Path,
    command: &[String],
) -> std::io::Result<std::process::Output> {
    Command::new(&command[0])
        .args(&command[1..])
        .current_dir(repository)
        .output()
}

fn git_dirty_paths(repository: &Path) -> Result<Vec<PathBuf>, PhaseExecutionError> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(repository)
        .output()
        .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
    if !output.status.success() {
        return Err(PhaseExecutionError::Seam(SeamError::new(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| path.split(" -> ").last().unwrap_or(path))
        .map(|path| repository.join(path.trim_matches('"')))
        .collect())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<String, PhaseExecutionError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .map_err(|error| PhaseExecutionError::Seam(SeamError::new(error.to_string())))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(PhaseExecutionError::Seam(SeamError::new(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )))
    }
}

fn matching_paths(
    repository: &Path,
    globs: &[String],
) -> Result<Vec<PathBuf>, PhaseExecutionError> {
    let mut files = Vec::new();
    collect_files(repository, repository, &mut files).map_err(|error| {
        PhaseExecutionError::Seam(SeamError::new(format!(
            "could not inspect residue paths: {error}"
        )))
    })?;
    files.retain(|path| {
        let relative = path.strip_prefix(repository).unwrap_or(path);
        let candidate = relative.to_string_lossy().replace('\\', "/");
        globs
            .iter()
            .any(|pattern| wildcard_match(pattern, &candidate))
    });
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == root.join(".git") {
            continue;
        }
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let (mut p, mut c, mut star, mut marked) = (0, 0, None, 0);
    while c < candidate.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == candidate[c]) {
            p += 1;
            c += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            while p < pattern.len() && pattern[p] == b'*' {
                p += 1;
            }
            star = Some(p);
            marked = c;
        } else if let Some(next) = star {
            marked += 1;
            c = marked;
            p = next;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn display_paths(repository: &Path, paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.strip_prefix(repository).unwrap_or(path))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn probe_version(executable: &str) -> std::io::Result<String> {
    let output = Command::new(executable).arg("--version").output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "version probe exited {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn normalize_version(value: &str) -> String {
    value
        .split_whitespace()
        .find(|part| {
            part.chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
        })
        .unwrap_or(value)
        .trim_start_matches('v')
        .to_owned()
}

fn version_less_than(observed: &str, minimum: &str) -> bool {
    version_parts(&normalize_version(observed)) < version_parts(&normalize_version(minimum))
}

fn version_parts(value: &str) -> Vec<u64> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
        })
}
