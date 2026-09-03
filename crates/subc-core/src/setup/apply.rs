use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Map, Value};

use super::{
    components::{self, ReleaseArtifactSource},
    config,
    conversion::selected_components,
    inventory::Inventory,
    model::{
        Component, ComponentState, ConfigurationState, PlatformObservation, ReleaseAvailability,
        RuntimeState, SetupObserved, SetupOperation, SetupRequest,
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
    /// Completed changes from this invocation, partitioned so a failure in one
    /// module never rewinds another module or the daemon runtime.
    component_steps: BTreeMap<Component, ComponentSteps>,
}

#[derive(Default)]
struct ComponentSteps {
    placed_binaries: Vec<PathBuf>,
    configured: bool,
    configuration_inventory_created: bool,
}

struct SetupPaths {
    data_dir: PathBuf,
    binary_home: PathBuf,
    config_path: PathBuf,
    claustrum_key_path: Option<PathBuf>,
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
                data_dir: data_dir.clone(),
                binary_home,
                config_path: subc_core::daemon_config::default_config_path(),
                // One default is retained and reused for bootstrap and generated
                // module environment so the vault never receives mismatched keys.
                claustrum_key_path: if cfg!(target_os = "macos") {
                    None
                } else {
                    Some(data_dir.join("claustrum").join("master.key"))
                },
                runtime_paths,
            },
            platform,
            inventory,
            runner: SystemCommandRunner,
            artifacts: ReleaseArtifactSource::current(),
            runtime_status: RuntimeStatus::default(),
            uninstall_report: None,
            component_steps: BTreeMap::new(),
        })
    }

    pub fn observe(&mut self, request: &SetupRequest) -> Result<SetupObserved, String> {
        self.runtime_status = runtime::observe(self.platform, &mut self.runner)?;
        let selected = selected_components(request);
        let claustrum_key_path = request
            .claustrum_key_path
            .as_deref()
            .or(self.paths.claustrum_key_path.as_deref());
        let registered_modules = self.live_enabled_modules()?;
        let mut components = BTreeMap::new();
        let mut releases = BTreeMap::new();
        // A failed index is about the document, not a single component: setup
        // must not plan any installation from it.
        self.artifacts.ensure_index()?;
        for component in Component::ALL {
            if matches!(
                PlatformObservation::current(),
                PlatformObservation::Supported(target) if component.is_declared_unsupported_on(target)
            ) {
                components.insert(component, ComponentState::Missing);
                releases.insert(component, ReleaseAvailability::NotRequired);
                continue;
            }
            let config_ok = match components::configuration_is_correct(
                component,
                &self.paths.config_path,
                &self.paths.binary_home,
                claustrum_key_path,
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
                observed_component_state(
                    component,
                    config_ok,
                    binary_ok,
                    registered_modules.as_ref(),
                ),
            );
            // A component the index does not list is a fact about that
            // component, never about the plan: one unpublished module must not
            // abort installing everything else.
            let availability = match self.artifacts.release_availability(component) {
                Ok(availability) => availability,
                Err(reason) => ReleaseAvailability::Unresolvable { reason },
            };
            releases.insert(component, availability);
        }
        let configuration = selected
            .iter()
            .copied()
            .find_map(|component| {
                match config::plan_component_with_key(
                    &self.paths.config_path,
                    component,
                    &self.paths.binary_home,
                    claustrum_key_path,
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
        observed.running_ck_adoption = self.running_ck_adoption();
        // AFT automatic detection is disabled for alpha until its owner supplies
        // a marker contract with false-positive classification rules.
        observed.detections.remove(&Component::Aft);
        Ok(observed)
    }

    pub fn print_proposed_diffs(
        &self,
        plan: &SetupPlan,
        request: &SetupRequest,
    ) -> Result<(), String> {
        for operation in &plan.operations {
            let SetupOperation::ConfigureComponent { component } = operation else {
                continue;
            };
            if let Some(change) = config::plan_component_with_key(
                &self.paths.config_path,
                *component,
                &self.paths.binary_home,
                request
                    .claustrum_key_path
                    .as_deref()
                    .or(self.paths.claustrum_key_path.as_deref()),
            )
            .map_err(|conflict| {
                format!(
                    "refusal: conflicting user-owned configuration key '{}'",
                    conflict.key
                )
            })? {
                println!("proposed configuration diff:\n{}", change.render_diff());
            }
        }
        Ok(())
    }

    pub fn apply_plan(&mut self, plan: &SetupPlan, request: &SetupRequest) -> Result<(), String> {
        if let Some(key_path) = &request.claustrum_key_path {
            self.paths.claustrum_key_path = Some(key_path.clone());
        }
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
        validation::validate_selected(&mut validator, &selected)?;
        println!("{}", validation::MCP_HARNESS_SNIPPET);
        Ok(())
    }

    fn run_ck(&self, args: &[&str]) -> Result<(), String> {
        let status = Command::new(&self.executable)
            .args(args)
            .status()
            .map_err(|error| format!("could not run ck {}: {error}", args.join(" ")))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("ck {} failed with {status}", args.join(" ")))
        }
    }

    /// Returns enabled module ids only when the daemon is live. A stopped daemon
    /// has no observable registry and will load the managed configuration on start.
    fn live_enabled_modules(&self) -> Result<Option<BTreeSet<String>>, String> {
        if !self.runtime_status.live {
            return Ok(None);
        }
        let output = Command::new(&self.executable)
            .args(["--json", "module", "list"])
            .output()
            .map_err(|error| format!("could not run ck module list --json: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "ck module list --json failed with {}",
                output.status
            ));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("ck module list --json returned invalid JSON: {error}"))?;
        let entries = value
            .get("modules")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())
            .ok_or_else(|| "ck module list --json omitted modules".to_string())?;
        Ok(Some(
            entries
                .iter()
                .filter(|entry| entry.get("enabled").and_then(Value::as_bool) == Some(true))
                .filter_map(|entry| entry.get("module_id").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect(),
        ))
    }

    fn running_ck_adoption(&self) -> Option<PathBuf> {
        let binary_home = fs::canonicalize(&self.paths.binary_home)
            .unwrap_or_else(|_| self.paths.binary_home.clone());
        self.executable
            .starts_with(binary_home)
            .then(|| self.executable.clone())
            .filter(|path| path.is_file())
            .filter(|path| !self.inventory.owns_path("managed-binary", path))
    }

    fn adopt_running_ck(&mut self, path: &Path) -> Result<(), String> {
        if path != self.executable || self.running_ck_adoption().as_deref() != Some(path) {
            return Err(format!(
                "refusal: running ck at {} is not an unowned bootstrap placement",
                path.display()
            ));
        }
        let mut fields = Map::new();
        fields.insert(
            "component".to_string(),
            Value::String(Component::Core.label().to_string()),
        );
        fields.insert(
            "sha256".to_string(),
            Value::String(components::digest_file(path)?),
        );
        // Bootstrap installers hash the extracted ck into `sha256` (ownership).
        // Currency needs the zip digest, recorded as `archive_sha256` when the
        // installer has started writing that field. Copy it when present; leave
        // it absent otherwise so the next upgrade establishes it once.
        if let Some(archive) = self
            .inventory
            .entry_for_path("binary-placement", path)
            .and_then(|entry| entry.get("archive_sha256"))
            .and_then(Value::as_str)
        {
            fields.insert(
                "archive_sha256".to_string(),
                Value::String(archive.to_string()),
            );
        }
        fields.insert(
            "version".to_string(),
            Value::String(super::upgrade::binary_version(path)?),
        );
        self.inventory.record("managed-binary", path, fields);
        self.inventory.save()?;
        println!(
            "adopted: {} (placed by the bootstrap installer)",
            path.display()
        );
        Ok(())
    }

    fn bootstrap_claustrum(&self, key_path: Option<&Path>) -> Result<(), String> {
        let auth = self.paths.binary_home.join(if cfg!(windows) {
            "ck-auth.exe"
        } else {
            "ck-auth"
        });
        let mut command = Command::new(auth);
        command.arg("bootstrap");
        if let Some(key_path) = key_path {
            command.arg("--key-path").arg(key_path);
        }
        let status = command
            .status()
            .map_err(|error| format!("could not run ck auth bootstrap: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("ck auth bootstrap failed with {status}"))
        }
    }
}

impl SetupExecutor for SetupBackend {
    type Error = String;

    fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
        match operation {
            SetupOperation::InstallComponent { component } => {
                let paths = components::component_binary_paths(*component, &self.paths.binary_home);
                let owned_before = paths
                    .iter()
                    .map(|path| self.inventory.owns_path("managed-binary", path))
                    .collect::<Vec<_>>();
                components::install_component(
                    *component,
                    &self.paths.binary_home,
                    &mut self.inventory,
                    &mut self.artifacts,
                )?;
                let steps = self.component_steps.entry(*component).or_default();
                for (path, owned_before) in paths.into_iter().zip(owned_before) {
                    if !owned_before && self.inventory.owns_path("managed-binary", &path) {
                        steps.placed_binaries.push(path);
                    }
                }
                Ok(())
            }
            SetupOperation::ConfigureComponent { component } => {
                let configuration_inventory_created = !self
                    .inventory
                    .owns_path("configuration", &self.paths.config_path);
                let changed = components::configure_component(
                    *component,
                    &self.paths.config_path,
                    &self.paths.binary_home,
                    self.paths.claustrum_key_path.as_deref(),
                    &mut self.inventory,
                )?
                .is_some();
                if changed {
                    let steps = self.component_steps.entry(*component).or_default();
                    steps.configured = true;
                    steps.configuration_inventory_created = configuration_inventory_created;
                }
                Ok(())
            }
            SetupOperation::BootstrapClaustrum { key_path } => {
                self.bootstrap_claustrum(key_path.as_deref())
            }
            SetupOperation::AdoptRunningCk { path } => self.adopt_running_ck(path),
            SetupOperation::RescanComponent { .. } => self.run_ck(&["module", "rescan"]),
            SetupOperation::EnableComponent { component } => self.run_ck(&[
                "module",
                "start",
                component
                    .module_id()
                    .expect("only modules are enabled by setup"),
            ]),
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

    fn rollback_component(&mut self, component: Component) -> Result<(), Self::Error> {
        let Some(steps) = self.component_steps.remove(&component) else {
            return Ok(());
        };
        for path in steps.placed_binaries.into_iter().rev() {
            if self.inventory.owns_path("managed-binary", &path) {
                match fs::remove_file(&path) {
                    Ok(()) => println!("rolled back: {}", path.display()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("could not roll back {}: {error}", path.display()))
                    }
                }
                self.inventory.remove_owned_path("managed-binary", &path);
            }
        }
        if steps.configured
            && config::remove_component(
                &self.paths.config_path,
                component,
                &self.paths.binary_home,
                self.paths.claustrum_key_path.as_deref(),
            )?
        {
            println!("rolled back: {}", self.paths.config_path.display());
        }
        if steps.configuration_inventory_created
            && !self.component_steps.values().any(|other| other.configured)
        {
            self.inventory
                .remove_owned_path("configuration", &self.paths.config_path);
        }
        self.inventory.save()
    }
}

fn observed_component_state(
    component: Component,
    config_ok: bool,
    binary_ok: bool,
    registered_modules: Option<&BTreeSet<String>>,
) -> ComponentState {
    if !config_ok || !binary_ok {
        return ComponentState::Missing;
    }
    if let (Some(module_id), Some(registered)) = (component.module_id(), registered_modules) {
        return if registered.contains(module_id) {
            ComponentState::Correct
        } else {
            ComponentState::Configured
        };
    }
    // A daemon that is not live cannot report its in-memory module registry.
    // Starting it will reconcile the already-correct file.
    ComponentState::Correct
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

pub fn default_claustrum_key_path() -> Result<Option<PathBuf>, String> {
    if cfg!(target_os = "macos") {
        Ok(None)
    } else {
        Ok(Some(data_directory()?.join("claustrum").join("master.key")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_daemon_missing_an_otherwise_correct_module_is_configured() {
        let registered = BTreeSet::new();

        assert_eq!(
            observed_component_state(Component::Aft, true, true, Some(&registered)),
            ComponentState::Configured
        );
        assert_eq!(
            observed_component_state(Component::Aft, true, true, None),
            ComponentState::Correct,
            "a stopped daemon cannot expose its registry"
        );
    }
}

#[cfg(all(test, unix))]
mod adoption_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use subc_core::test_support::TestTempDir;

    #[test]
    fn bootstrap_placed_ck_is_adopted_as_managed_binary() {
        let root = TestTempDir::new("setup-adopt-running-ck");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).unwrap();
        let placed = binary_home.join("ck");
        fs::write(&placed, "#!/bin/sh\necho 'ck 0.16.2'\n").unwrap();
        fs::set_permissions(&placed, fs::Permissions::from_mode(0o755)).unwrap();
        let executable = fs::canonicalize(&placed).unwrap();
        let platform = RuntimePlatform::current();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        inventory.record("binary-placement", &executable, Map::new());
        let mut backend = SetupBackend {
            executable: executable.clone(),
            paths: SetupPaths {
                data_dir: root.join("data"),
                binary_home: binary_home.clone(),
                config_path: root.join("subc.jsonc"),
                claustrum_key_path: None,
                runtime_paths: runtime::runtime_paths(platform, &binary_home, &root),
            },
            platform,
            inventory,
            runner: SystemCommandRunner,
            artifacts: ReleaseArtifactSource::from_index(
                super::super::release_index::ReleaseIndex {
                    schema: 1,
                    channel: "alpha".to_string(),
                    generated_at_ms: 0,
                    components: BTreeMap::new(),
                },
                super::super::model::AlphaTarget::LinuxX64,
            ),
            runtime_status: RuntimeStatus::default(),
            uninstall_report: None,
            component_steps: BTreeMap::new(),
        };

        backend.adopt_running_ck(&executable).unwrap();

        assert!(backend.inventory.owns_path("managed-binary", &executable));
        assert!(backend.inventory.owns_path("binary-placement", &executable));
        assert_eq!(
            backend
                .inventory
                .entry_for_path("managed-binary", &executable)
                .and_then(|entry| entry.get("version"))
                .and_then(Value::as_str),
            Some("0.16.2")
        );
        assert!(
            backend
                .inventory
                .entry_for_path("managed-binary", &executable)
                .and_then(|entry| entry.get("archive_sha256"))
                .is_none(),
            "bootstrap row without archive_sha256 must not invent one"
        );
    }

    #[test]
    fn adoption_copies_bootstrap_archive_digest_when_present() {
        let root = TestTempDir::new("setup-adopt-archive-digest");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).unwrap();
        let placed = binary_home.join("ck");
        fs::write(&placed, "#!/bin/sh\necho 'ck 0.16.2'\n").unwrap();
        fs::set_permissions(&placed, fs::Permissions::from_mode(0o755)).unwrap();
        let executable = fs::canonicalize(&placed).unwrap();
        let platform = RuntimePlatform::current();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        let archive_digest = "cd".repeat(32);
        let mut bootstrap = Map::new();
        bootstrap.insert(
            "archive_sha256".to_string(),
            Value::String(archive_digest.clone()),
        );
        inventory.record("binary-placement", &executable, bootstrap);
        let mut backend = SetupBackend {
            executable: executable.clone(),
            paths: SetupPaths {
                data_dir: root.join("data"),
                binary_home: binary_home.clone(),
                config_path: root.join("subc.jsonc"),
                claustrum_key_path: None,
                runtime_paths: runtime::runtime_paths(platform, &binary_home, &root),
            },
            platform,
            inventory,
            runner: SystemCommandRunner,
            artifacts: ReleaseArtifactSource::from_index(
                super::super::release_index::ReleaseIndex {
                    schema: 1,
                    channel: "alpha".to_string(),
                    generated_at_ms: 0,
                    components: BTreeMap::new(),
                },
                super::super::model::AlphaTarget::LinuxX64,
            ),
            runtime_status: RuntimeStatus::default(),
            uninstall_report: None,
            component_steps: BTreeMap::new(),
        };

        backend.adopt_running_ck(&executable).unwrap();

        let adopted = backend
            .inventory
            .entry_for_path("managed-binary", &executable)
            .expect("adopted row");
        assert_eq!(
            adopted.get("archive_sha256").and_then(Value::as_str),
            Some(archive_digest.as_str())
        );
        let binary_digest = components::digest_file(&executable).unwrap();
        assert_eq!(
            adopted.get("sha256").and_then(Value::as_str),
            Some(binary_digest.as_str())
        );
        assert_ne!(binary_digest, archive_digest);
    }
}
