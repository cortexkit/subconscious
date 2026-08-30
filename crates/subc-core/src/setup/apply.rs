use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::Command,
};

use super::{
    components::{self, ReleaseArtifactSource},
    config,
    conversion::selected_components,
    inventory::Inventory,
    model::{
        Component, ComponentState, ConfigurationState, DetectionOutcome, PlatformObservation,
        ReleaseAvailability, RuntimeState, SetupObserved, SetupOperation, SetupRequest,
    },
    planner::{execute_setup, ExecutionMode, SetupExecutor, SetupPlan},
    runtime::{self, RuntimePlatform, RuntimeStatus, SystemCommandRunner},
    uninstall,
    validation::{self, Validator},
};

pub struct SetupBackend {
    executable: PathBuf,
    paths: SetupPaths,
    platform: RuntimePlatform,
    inventory: Inventory,
    runner: SystemCommandRunner,
    artifacts: ReleaseArtifactSource,
    runtime_status: RuntimeStatus,
    uninstall_report: Option<uninstall::UninstallReport>,
}

struct SetupPaths {
    data_dir: PathBuf,
    binary_home: PathBuf,
    config_path: PathBuf,
    runtime_paths: runtime::RuntimePaths,
}

impl SetupBackend {
    pub fn current(executable: impl Into<PathBuf>) -> Result<Self, String> {
        let executable = executable.into();
        let platform = RuntimePlatform::current();
        let data_dir = data_directory()?;
        let binary_home = data_dir.join("bin");
        let runtime_home = user_home()?;
        let runtime_paths = runtime::runtime_paths(platform, &binary_home, &runtime_home);
        let inventory = Inventory::load(
            data_dir.join("installer-manifest.json"),
            PlatformObservation::current().to_string().as_str(),
        )?;
        Ok(Self {
            executable,
            paths: SetupPaths {
                data_dir,
                binary_home,
                config_path: subc_core::daemon_config::default_config_path(),
                runtime_paths,
            },
            platform,
            inventory,
            runner: SystemCommandRunner,
            artifacts: ReleaseArtifactSource::current(),
            runtime_status: RuntimeStatus::default(),
            uninstall_report: None,
        })
    }

    pub fn observe(&mut self, request: &SetupRequest) -> Result<SetupObserved, String> {
        self.runtime_status = runtime::observe(self.platform, &mut self.runner)?;
        let selected = selected_components(request);
        let mut components = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for component in Component::ALL {
            let config_ok = match components::configuration_is_correct(
                component,
                &self.paths.config_path,
                &self.paths.binary_home,
            ) {
                Ok(correct) => correct,
                // A component outside this request may have a user-owned value
                // different from CortexKit's managed value. It must not block or
                // be rewritten while another component is being added.
                Err(_) if !selected.contains(&component) => false,
                Err(error) => return Err(error),
            };
            let binary_ok =
                components::is_installed(component, &self.paths.binary_home, &self.inventory);
            components.insert(
                component,
                if config_ok && binary_ok {
                    ComponentState::Correct
                } else {
                    ComponentState::Missing
                },
            );
            releases.insert(
                component,
                if component == Component::Mc {
                    ReleaseAvailability::NotRequired
                } else {
                    ReleaseAvailability::Available
                },
            );
        }
        let configuration = selected
            .iter()
            .copied()
            .find_map(|component| {
                match config::plan_component(
                    &self.paths.config_path,
                    component,
                    &self.paths.binary_home,
                ) {
                    Err(conflict) => Some(ConfigurationState::Conflict { key: conflict.key }),
                    Ok(_) => None,
                }
            })
            .unwrap_or(ConfigurationState::Additive);
        let mut observed = SetupObserved::unconfigured_current_host();
        observed.components = components;
        observed.releases = releases;
        observed.runtime = if self.runtime_status.registered
            && self.runtime_status.live
            && self
                .inventory
                .owns_path("runtime-definition", &self.paths.runtime_paths.definition)
        {
            RuntimeState::Correct
        } else {
            RuntimeState::Missing
        };
        observed.configuration = configuration;
        observed
            .detections
            .retain(|component, _| *component != Component::Aft);
        observed.detections.insert(
            Component::Aft,
            DetectionOutcome::OwnerGated {
                reason: "automatic AFT detection is disabled until its owner supplies a detector contract".to_string(),
            },
        );
        Ok(observed)
    }

    pub fn print_proposed_diffs(&self, plan: &SetupPlan) -> Result<(), String> {
        for operation in &plan.operations {
            let SetupOperation::ConfigureComponent { component } = operation else {
                continue;
            };
            if let Some(change) =
                config::plan_component(&self.paths.config_path, *component, &self.paths.binary_home)
                    .map_err(|conflict| {
                        format!(
                            "refusal: conflicting user-owned configuration key '{}'",
                            conflict.key
                        )
                    })?
            {
                println!("proposed configuration diff:\n{}", change.render_diff());
            }
        }
        Ok(())
    }

    pub fn apply_plan(&mut self, plan: &SetupPlan, request: &SetupRequest) -> Result<(), String> {
        execute_setup(plan, ExecutionMode::Apply, self)?;
        self.inventory.save()?;
        if request.uninstall {
            let report =
                self.uninstall_report
                    .take()
                    .unwrap_or_else(|| uninstall::UninstallReport {
                        removed: Vec::new(),
                        retained: vec![
                            format!("configuration: {}", self.paths.config_path.display()),
                            format!("store: {}", self.paths.data_dir.join("stores").display()),
                        ],
                    });
            println!("retained after uninstall:");
            for retained in report.retained {
                println!("  {retained}");
            }
            return Ok(());
        }
        if plan.mutation_count() == 0 {
            println!("setup: no action was needed; managed setup is already correct.");
        }
        let selected = selected_components(request).into_iter().collect::<Vec<_>>();
        let mut validator = CkValidator {
            executable: &self.executable,
        };
        validation::validate_selected(&mut validator, &selected, &self.paths.config_path)?;
        println!("{}", validation::MCP_HARNESS_SNIPPET);
        Ok(())
    }
}

impl SetupExecutor for SetupBackend {
    type Error = String;

    fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
        match operation {
            SetupOperation::InstallComponent { component } => components::install_component(
                *component,
                &self.paths.binary_home,
                &mut self.inventory,
                &mut self.artifacts,
            ),
            SetupOperation::ConfigureComponent { component } => {
                components::configure_component(
                    *component,
                    &self.paths.config_path,
                    &self.paths.binary_home,
                    &mut self.inventory,
                )?;
                Ok(())
            }
            SetupOperation::RegisterRuntime => runtime::ensure(
                self.platform,
                &self.paths.runtime_paths,
                self.runtime_status,
                &mut self.runner,
                &mut self.inventory,
            ),
            // Registration starts the daemon immediately on every platform. The
            // separate operation remains in the plan so its current-liveness
            // requirement is visible before execution.
            SetupOperation::StartRuntime => Ok(()),
            SetupOperation::DeregisterRuntime => Ok(()),
            SetupOperation::RemoveManagedComponent { .. } if self.uninstall_report.is_none() => {
                let report = uninstall::uninstall(
                    self.platform,
                    &self.paths.runtime_paths,
                    &mut self.runner,
                    &mut self.inventory,
                    &self.paths.config_path,
                    &[self.paths.data_dir.join("stores")],
                )?;
                self.uninstall_report = Some(report);
                Ok(())
            }
            SetupOperation::RemoveManagedComponent { .. }
            | SetupOperation::ObservePlatform
            | SetupOperation::OfferOptionalComponents
            | SetupOperation::OfferConversion { .. }
            | SetupOperation::ConfirmConversion { .. }
            | SetupOperation::Validate { .. }
            | SetupOperation::RetainUserData => Ok(()),
        }
    }
}

struct CkValidator<'a> {
    executable: &'a Path,
}

impl Validator for CkValidator<'_> {
    fn run(&mut self, label: &str, args: &[String]) -> Result<bool, String> {
        let status = Command::new(self.executable)
            .args(args)
            .status()
            .map_err(|error| format!("could not run {label} {}: {error}", args.join(" ")))?;
        Ok(status.success())
    }
}

fn data_directory() -> Result<PathBuf, String> {
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("cortexkit"))
            .ok_or_else(|| "LOCALAPPDATA is unavailable for user-scoped setup".to_string());
    }
    if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(data_home).join("cortexkit"));
    }
    Ok(user_home()?.join(".local").join("share").join("cortexkit"))
}

fn user_home() -> Result<PathBuf, String> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "the user home directory is unavailable for setup".to_string())
}
