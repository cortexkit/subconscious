use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::cli_output::println;
use serde_json::{Map, Value};

use super::{
    bus,
    bus_monitoring::{self, BusMonitoring},
    ckni,
    components::{self, ReleaseArtifactSource},
    config,
    conversion::selected_components,
    file_modes,
    inventory::Inventory,
    model::{
        Component, ComponentState, CoreVersion, PartialInstall, PlatformObservation,
        ReleaseAvailability, SetupObserved, SetupOperation, SetupRequest,
    },
    planner::{SetupExecutor, SetupPlan},
    runtime::{self, CommandRunner, RuntimePlatform, RuntimeStatus, SystemCommandRunner},
    uninstall,
    validation::{self, Validator},
};

type RescanPreview = fn() -> Result<Vec<String>, String>;

mod broca;
pub(in crate::setup) mod extension;
mod journal;
mod module_steps;
#[cfg(test)]
mod module_steps_tests;
mod plexus_policy;
mod settings;
pub(super) mod share_keys;
pub(super) mod vault;

pub struct SetupBackend {
    executable: PathBuf,
    paths: SetupPaths,
    platform: RuntimePlatform,
    inventory: Inventory,
    runner: Box<dyn CommandRunner>,
    bus_control: Box<dyn bus::BusControl>,
    artifacts: ReleaseArtifactSource,
    runtime_status: RuntimeStatus,
    uninstall_report: Option<uninstall::UninstallReport>,
    /// Completed changes from this invocation, partitioned so a failure in one
    /// module never rewinds another module or the daemon runtime.
    component_steps: BTreeMap<Component, ComponentSteps>,
    rescan_preview: Option<RescanPreview>,
}

#[derive(Default)]
struct ComponentSteps {
    message_hub_change: Option<config::ConfigChange>,
    message_grants_changed: bool,
    message_bus_changed: bool,
    plexus_maintenance_changed: bool,
    nats_migration: Option<super::nats_migration::Migration>,
    failed_bootstrap_directory: Option<BootstrapDirectory>,
    module_step_failed: bool,
}

struct BootstrapDirectory {
    path: PathBuf,
    existed_before: bool,
}

impl BootstrapDirectory {
    fn capture(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("claustrum");
        let existed_before = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
        };
        Ok(Self {
            path,
            existed_before,
        })
    }

    fn rollback(self) -> Result<String, String> {
        // A failed bootstrap may have opened store.db before refusing a key.
        // Only a directory absent before that attempt belongs to this rollback.
        if self.existed_before {
            return Ok(format!(
                "setup left existing vault directory in place: {}",
                self.path.display()
            ));
        }
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not roll back {}: {error}",
                    self.path.display()
                ))
            }
        }
        Ok(format!("rolled back: {}", self.path.display()))
    }
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
        Self::current_with_preview(executable, false)
    }

    #[doc(hidden)]
    pub fn current_with_preview(
        executable: impl Into<PathBuf>,
        preview: bool,
    ) -> Result<Self, String> {
        let executable = executable.into();
        let platform = RuntimePlatform::current();
        let data_dir = data_directory()?;
        let binary_home = data_dir.join("bin");
        let runtime_home = user_home()?;
        let runtime_paths = runtime::session_runtime_paths(platform, &binary_home, &runtime_home);
        // The lock comes first, before anything setup observes or plans, and
        // as this process's hold: anything setup then loads under the lock
        // shares it rather than waiting on it. The inventory keeps it held.
        let manifest = data_dir.join("installer-manifest.json");
        let platform_label = PlatformObservation::current().to_string();
        let inventory = if preview {
            match super::install_journal::Journal::status(&manifest)? {
                Some(super::install_journal::Status::Open) => println!(
                    "An unfinished install transaction is recorded at {}.",
                    super::install_journal::Journal::path(&manifest).display()
                ),
                Some(super::install_journal::Status::NeedsCleanup) => println!(
                    "A completed install transaction has leftovers listed in {}.",
                    super::install_journal::Journal::path(&manifest).display()
                ),
                Some(super::install_journal::Status::Retired) => println!(
                    "Completed install transactions still have leftovers listed in {}.",
                    super::install_journal::retired::path(&manifest).display()
                ),
                None => {}
            }
            Inventory::load_snapshot(manifest, &platform_label)?
        } else {
            let _hold = super::install_lock::InstallHold::take(
                &manifest,
                "ck setup",
                super::install_lock::WAIT,
            )?;
            super::install_journal::system::recover_locked(
                &manifest,
                &executable,
                || None,
                &runtime_paths,
                &mut SystemCommandRunner,
            )?;
            let current = Inventory::read(manifest.clone(), &platform_label)?;
            super::install_journal::bootstrap::refuse_legacy(&manifest, &current)?;
            #[cfg(unix)]
            super::install_journal::profile::from_bootstrap(&manifest, &executable)?;
            #[cfg(windows)]
            super::install_journal::user_path::from_bootstrap(&manifest, &executable)?;
            Inventory::load_locked(
                manifest,
                &platform_label,
                "ck setup",
                super::install_lock::WAIT,
            )?
        };
        Ok(Self {
            bus_control: super::bus_control(&executable),
            executable,
            paths: SetupPaths {
                data_dir: data_dir.clone(),
                binary_home,
                config_path: subc_daemon::daemon_config::default_config_path(),
                // The vault chooses its own OS key store and refuses a key file
                // inside its data folder. Only an operator may select a file.
                claustrum_key_path: default_claustrum_key_path()?,
                runtime_paths,
            },
            platform,
            inventory,
            runner: Box::new(SystemCommandRunner),
            artifacts: ReleaseArtifactSource::current(),
            runtime_status: RuntimeStatus::default(),
            uninstall_report: None,
            component_steps: BTreeMap::new(),
            rescan_preview: None,
        })
    }

    /// Use the caller's authenticated client for previews, without spawning a
    /// second CLI process that could connect to a different daemon.
    pub fn set_rescan_preview(&mut self, preview: RescanPreview) {
        self.rescan_preview = Some(preview);
    }

    pub fn observe(&mut self, request: &SetupRequest) -> Result<SetupObserved, String> {
        if !request.uninstall {
            // `CK_TEST_SETUP_PREFLIGHT_OK` skips only this check, for tests that
            // exercise the real setup path on a Linux CI runner, which has no
            // systemd user manager to satisfy it.
            #[cfg(feature = "test-support")]
            let stub_runtime = env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some()
                || env::var_os("CK_TEST_SETUP_PREFLIGHT_OK").is_some();
            #[cfg(not(feature = "test-support"))]
            let stub_runtime = false;
            if !stub_runtime {
                runtime::preflight(self.platform, &mut self.runner)?;
            }
        }

        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            // `CK_TEST_SETUP_CONTROL_OK` is the test-support control that stubs
            // out setup's calls to the real platform. Here it replaces observing
            // the daemon's service registration: the runtime is reported
            // registered and live, or, with `CK_TEST_SETUP_RUNTIME_MISSING`, not
            // registered at all. The second state lets a test run fresh setup that
            // registers the runtime and then validates it, without the stub
            // claiming the runtime was registered before setup began.
            self.runtime_status = if env::var_os("CK_TEST_SETUP_RUNTIME_MISSING").is_some() {
                RuntimeStatus::default()
            } else {
                RuntimeStatus {
                    registered: true,
                    live: true,
                }
            };
        } else {
            self.runtime_status = runtime::observe(self.platform, &mut self.runner)?;
        }
        #[cfg(not(feature = "test-support"))]
        {
            self.runtime_status = runtime::observe(self.platform, &mut self.runner)?;
        }
        let selected = selected_components(request);
        self.artifacts.set_download_context(
            request
                .optional_components
                .first()
                .copied()
                .or(request.convert)
                .unwrap_or(Component::Core),
            request.verbose,
        );
        let claustrum_key_path = request
            .claustrum_key_path
            .as_deref()
            .or(self.paths.claustrum_key_path.as_deref());
        let registered_modules = self.live_enabled_modules()?;
        let mut components = BTreeMap::new();
        let mut releases = BTreeMap::new();
        let mut unowned_binaries = BTreeMap::new();
        let mut configuration_refusals = BTreeMap::new();
        let mut partial_installs = BTreeMap::new();
        // A failed index is about the document, not a single component: setup
        // must not plan any installation from it.
        self.artifacts.ensure_index()?;
        let release_index = self.artifacts.cloned_index()?;
        let requires_core = Component::ALL
            .into_iter()
            .filter_map(|component| {
                release_index
                    .components
                    .get(component.label())
                    .and_then(|entry| entry.requires_core.clone())
                    .map(|floor| (component, floor))
            })
            .collect::<BTreeMap<_, _>>();
        let nats_pending = !request.uninstall
            && self
                .legacy_nats_migration()?
                .is_some_and(|m| m.needs_rewrite());
        for component in Component::ALL {
            if matches!(
                PlatformObservation::current(),
                PlatformObservation::Supported(target) if component.is_declared_unsupported_on(target)
            ) {
                components.insert(component, ComponentState::Missing);
                let availability = if release_index.components.contains_key(component.label()) {
                    ReleaseAvailability::NotRequired
                } else {
                    self.artifacts.release_availability(component)?
                };
                releases.insert(component, availability);
                continue;
            }
            let config_ok = match config::pending_dotted_keys(
                &self.paths.config_path,
                component,
                &self.paths.binary_home,
                claustrum_key_path,
            ) {
                // Upgrade repairs existing declarations. A missing declaration
                // alone does not make setup reinstall an otherwise configured component.
                Ok(keys) => keys
                    .iter()
                    .all(|key| key.ends_with(".registration_channel")),
                Err(conflict) => {
                    if selected.contains(&component) {
                        configuration_refusals.insert(
                            component,
                            if conflict.key == self.paths.config_path.display().to_string() {
                                format!(
                                    "could not check configuration at {}",
                                    self.paths.config_path.display()
                                )
                            } else {
                                format!(
                                    "user-owned configuration conflicts at key '{}'",
                                    conflict.key
                                )
                            },
                        );
                    }
                    false
                }
            } && !(component == Component::Core && nats_pending);
            let binary_ok =
                components::is_installed(component, &self.paths.binary_home, &self.inventory);
            if !request.uninstall && selected.contains(&component) && !binary_ok {
                if component.stops_before_upgrade()
                    && (registered_modules
                        .as_ref()
                        .is_some_and(|modules| modules.contains(component.label()))
                        || components::installation_binary_paths(
                            component,
                            &self.paths.binary_home,
                        )
                        .iter()
                        .any(|path| {
                            components::OWNED_BINARY_KINDS
                                .iter()
                                .any(|kind| self.inventory.owns_path(kind, path))
                        }))
                {
                    // Setup only installs a new pair. Adding one binary from
                    // a newer release could break the module already on disk.
                    configuration_refusals.insert(component, format!("{component} has an incomplete managed installation; restore both binaries from its original release before upgrading"));
                }
                if let PlatformObservation::Supported(target) = PlatformObservation::current() {
                    if let Some((missing_commands, module_matches_release)) =
                        components::partial_install(
                            component,
                            &self.paths.binary_home,
                            &self.inventory,
                            &release_index,
                            target,
                        )
                    {
                        partial_installs.insert(
                            component,
                            PartialInstall {
                                missing_commands,
                                module_matches_release,
                                module_state: observed_component_state(
                                    component,
                                    config_ok,
                                    true,
                                    registered_modules.as_ref(),
                                ),
                            },
                        );
                    }
                    unowned_binaries.insert(
                        component,
                        components::observe_unowned_binaries(
                            component,
                            &self.paths.binary_home,
                            &self.inventory,
                            &release_index,
                            target,
                        ),
                    );
                }
            }
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
        let mut observed = SetupObserved::unconfigured_current_host();
        observed.components = components;
        observed.releases = releases;
        observed.unowned_binaries = unowned_binaries;
        observed.available_core_version = release_index
            .components
            .get("core")
            .and_then(|entry| entry.version.clone());
        if !request.uninstall && selected.contains(&Component::Claustrum) {
            observed.claustrum_release = release_index.components.get("claustrum").map(|entry| {
                let version = entry.version.as_deref().and_then(|version| {
                    CoreVersion::from_release(version.trim_start_matches('v')).ok()
                });
                (entry.release.clone(), version)
            });
            observed.claustrum_binaries_owned = components::is_installed(
                Component::Claustrum,
                &self.paths.binary_home,
                &self.inventory,
            );
            let auth = self.auth_path();
            if auth.is_file() && owns_binary(&self.inventory, &auth) {
                observed.owned_auth_version = Some(
                    super::upgrade::binary_version(&auth)
                        .map_err(|_| ())
                        .and_then(|version| CoreVersion::from_release(&version)),
                );
            }
        }
        observed.requires_core = requires_core;
        observed.core_binary_present =
            self.paths.runtime_paths.daemon.is_file() || self.runtime_status.live;
        if observed.core_binary_present && !request.uninstall {
            observed.installed_core_version = self.observed_core_version();
        }
        observed.runtime = runtime::definition_state(
            self.platform,
            &self.paths.runtime_paths,
            self.runtime_status,
            &mut self.runner,
            &self.inventory,
        )?;
        observed.configuration_refusals = configuration_refusals;
        observed.partial_installs = partial_installs;
        observed.running_ck_adoption = self.running_ck_adoption();
        observed.ckni = self.ckni_observation(observed.running_ck_adoption.is_some());
        observed.has_bus_trust = self.has_bus_trust();
        if !request.uninstall && !observed.has_bus_trust {
            observed.bus_monitoring = bus_monitoring::observe(
                &self.paths.config_path,
                &mut OwnedBusMonitor(&self.inventory),
            );
            if let BusMonitoring::Skipped { .. } = &observed.bus_monitoring {
                println!("{}", observed.bus_monitoring);
            }
        }
        // AFT automatic detection is disabled for alpha until its owner supplies
        // a marker contract with false-positive classification rules.
        observed.detections.remove(&Component::Aft);
        // Dry-run cannot ask the live daemon: the new section is not on disk
        // yet, so a preview rescan always answers empty. Intersect the keys
        // core would write with the sections rescan cannot apply. The apply
        // path still previews after the write, when the file is the authority.
        let pending_keys = config::pending_dotted_keys(
            &self.paths.config_path,
            Component::Core,
            &self.paths.binary_home,
            claustrum_key_path,
        )
        .unwrap_or_default();
        observed.restart_required =
            config::restart_required_from_pending_keys(self.runtime_status.live, &pending_keys);
        if nats_pending && self.runtime_status.live {
            observed.restart_required.push("modules.nats-server".into());
        }
        observed.inventory_owned_paths = [
            "runtime-registration",
            "runtime-definition",
            "managed-link",
            "managed-binary",
            "binary-placement",
            extension::KIND,
        ]
        .iter()
        .map(|kind| self.inventory.paths_for_kind(kind).len())
        .sum();
        observed.permissions_need_tightening = self.permissions_need_tightening()?;
        if !request.uninstall {
            self.observe_module_steps(request, &release_index, &mut observed);
        }
        Ok(observed)
    }

    pub fn unmanaged_uninstall_notes(&self) -> Vec<String> {
        uninstall::unmanaged_uninstall_notes(self.platform, &self.inventory)
    }

    pub fn print_dry_run(&mut self, plan: &SetupPlan) -> Result<(), String> {
        for operation in &plan.operations {
            if let SetupOperation::InstallComponent { component } = operation {
                let release = self.artifacts.release_summary(*component)?;
                println!(
                    "Installing {} ({}, {})",
                    if component.plain_name().is_some() {
                        component.plan_name()
                    } else {
                        component.module_id().unwrap_or("CortexKit").to_string()
                    },
                    release.release,
                    release.target
                );
            }
        }

        let mut steps = Vec::new();
        if plan.permission_repair {
            steps.push("would tighten permissions on existing CortexKit paths".to_string());
        }
        let registering_runtime = plan
            .operations
            .iter()
            .any(|operation| matches!(operation, SetupOperation::RegisterRuntime));
        for operation in &plan.operations {
            match operation {
                SetupOperation::InstallComponent { component } => {
                    let release = self.artifacts.release_summary(*component)?;
                    for (binary, bytes) in release.assets {
                        let destination = self.paths.binary_home.join(if cfg!(windows) {
                            format!("{binary}.exe")
                        } else {
                            binary.to_string()
                        });
                        if components::is_new_component_module(*component, binary)
                            && destination.is_file()
                            && components::OWNED_BINARY_KINDS
                                .iter()
                                .any(|kind| self.inventory.owns_path(kind, &destination))
                        {
                            continue;
                        }
                        steps.push(format!(
                            "would download and verify {binary} ({})",
                            components::format_mebibytes(bytes)
                        ));
                        steps.push(format!(
                            "would place {}",
                            components::display_home_path(&destination)
                        ));
                    }
                }
                SetupOperation::ConfigureComponent { component } => steps.push(format!(
                    "would configure {} in {}",
                    component.module_id().unwrap_or(component.label()),
                    components::display_home_path(&self.paths.config_path)
                )),
                SetupOperation::BootstrapClaustrum { .. } => {
                    steps.push("would initialize claustrum credentials".to_string())
                }
                operation @ SetupOperation::ConfigureMessageBus => {
                    steps.push(operation.to_string())
                }
                operation @ (SetupOperation::GrantMessageKeys
                | SetupOperation::ConfigureMessageHub
                | SetupOperation::WriteModuleSettings { .. }
                | SetupOperation::EnsureBrocaKey { .. }
                | SetupOperation::GrantComponentVault { .. }
                | SetupOperation::RegisterCerebellumNativeHost
                | SetupOperation::EnsureCerebellumExtension { .. }
                | SetupOperation::EnsurePlexusCatalog { .. }
                | SetupOperation::EnablePlexusAgentPolicies) => {
                    steps.push(format!("would {operation}"));
                }
                SetupOperation::AdoptRunningCk { path } => steps.push(format!(
                    "would adopt {} as the managed ck binary",
                    components::display_home_path(path)
                )),
                SetupOperation::LinkCkni { link, .. } => steps.push(format!(
                    "would {} at {}",
                    ckni::Mechanism::CURRENT.plan_verb(),
                    components::display_home_path(link)
                )),
                SetupOperation::EnableComponent { component } => steps.push(format!(
                    "would register {} with the daemon and check its health",
                    component.module_id().unwrap_or(component.label())
                )),
                SetupOperation::RestartRuntime { sections } => steps.push(format!(
                    "would restart the daemon to apply {}",
                    sections.join(", ")
                )),
                SetupOperation::RegisterRuntime => {
                    steps.push("would register and start the daemon".to_string())
                }
                operation @ SetupOperation::UpdateRuntimeDefinition { .. } => {
                    steps.push(format!("would {operation}"));
                }
                SetupOperation::StartRuntime if !registering_runtime => {
                    steps.push("would start the daemon".to_string())
                }
                SetupOperation::DeregisterRuntime => {
                    steps.push("would stop and deregister the daemon".to_string())
                }
                SetupOperation::RemoveManagedComponent { component } => steps.push(format!(
                    "would remove managed {component} binaries and links"
                )),
                SetupOperation::RetainUserData => {
                    steps.push("would retain configuration and component stores".to_string())
                }
                SetupOperation::MonitorNatsServer { target } => {
                    steps.push(format!(
                        "would add nats-server's loopback monitoring listener to {} (ck-bus install-apply --conf-only)",
                        components::display_home_path(&target.nats_dir.join("server.conf"))
                    ));
                    steps.push(format!(
                        "would then add the nats-server health check to {}, unless one is already set",
                        components::display_home_path(&self.paths.config_path)
                    ));
                }
                SetupOperation::ObservePlatform
                | SetupOperation::OfferConversion { .. }
                | SetupOperation::ConfirmConversion { .. }
                | SetupOperation::RescanComponent { .. }
                | SetupOperation::StartRuntime
                | SetupOperation::Validate { .. } => {}
            }
        }
        for (index, step) in steps.iter().enumerate() {
            println!("  {}. {step}", index + 1);
        }
        Ok(())
    }

    pub fn print_proposed_diffs(
        &self,
        plan: &SetupPlan,
        request: &SetupRequest,
    ) -> Result<(), String> {
        if !request.dry_run {
            return Ok(());
        }
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
        self.execute_transactions(plan, request.uninstall)?;
        if request.uninstall {
            let report = match self.uninstall_report.take() {
                Some(report) => report,
                None => uninstall::uninstall_user_data(
                    self.platform,
                    &mut self.inventory,
                    &self.paths.config_path,
                    &[self.paths.data_dir.join("stores")],
                )?,
            };
            println!("retained after uninstall:");
            for retained in report.retained {
                println!("  {retained}");
            }
            #[cfg(windows)]
            match super::install_journal::Journal::retained_removals(self.inventory.manifest()) {
                Ok(images) => {
                    for (image, stage) in images {
                        println!("  image: {}", image.display());
                        println!(
                            "  staging directory: {} (cleaned on the next installer run)",
                            stage.display()
                        );
                    }
                }
                Err(error) => {
                    eprintln!("warning: could not list retained uninstall files: {error}")
                }
            }
            return Ok(());
        }
        for operation in &plan.operations {
            if let SetupOperation::EnableComponent { component } = operation {
                let module_id = component
                    .module_id()
                    .expect("setup enables only daemon modules");
                println!("  registered with the daemon; {module_id} ok");
            }
        }
        let maintenance_only = !plan.permission_repair
            && plan.operations.iter().filter(|op| op.mutates()).all(|op| {
                matches!(
                    op,
                    SetupOperation::GrantMessageKeys
                        | SetupOperation::ConfigureMessageBus
                        | SetupOperation::ConfigureMessageHub
                        | SetupOperation::GrantComponentVault {
                            component: Component::Plexus
                        }
                        | SetupOperation::EnablePlexusAgentPolicies
                )
            });
        let maintenance_changed = self.component_steps.values().any(|steps| {
            steps.message_hub_change.is_some()
                || steps.message_grants_changed
                || steps.message_bus_changed
                || steps.plexus_maintenance_changed
        });
        println!(
            "{}",
            if maintenance_only && !maintenance_changed {
                "Nothing changed."
            } else {
                "Done."
            }
        );
        if let Some(hint) = runtime::linger_hint(self.platform, &mut self.runner) {
            println!("{hint}");
        }
        if plan.operations.iter().any(|operation| {
            matches!(
                operation,
                SetupOperation::InstallComponent {
                    component: Component::Core
                }
            )
        }) {
            println!("{}", validation::MCP_HARNESS_NEXT);
        }
        Ok(())
    }

    fn run_ck(&self, args: &[&str]) -> Result<(), String> {
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            return Ok(());
        }
        let output = Command::new(&self.executable)
            .args(args)
            .output()
            .map_err(|error| format!("could not run ck {}: {error}", args.join(" ")))?;
        if output.status.success() {
            Ok(())
        } else {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if detail.is_empty() {
                format!("ck {} failed with {}", args.join(" "), output.status)
            } else {
                format!("ck {} failed: {detail}", args.join(" "))
            })
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.paths
            .binary_home
            .join(components::platform_binary("ck-auth"))
    }

    fn has_bus_trust(&self) -> bool {
        !self.inventory.paths_for_kind(bus::BUS_TRUST).is_empty()
    }

    fn legacy_nats_migration(&self) -> Result<Option<super::nats_migration::Migration>, String> {
        // The message-bus step repairs and verifies the managed wrapper after Core commits.
        if self.has_bus_trust() {
            return Ok(None);
        }
        super::nats_migration::Migration::capture(
            &self.paths.config_path,
            &self.paths.binary_home,
            &subc_daemon::bootstrap::connection_file_path(),
            &self.inventory,
        )
    }

    fn configure_message_bus(&mut self) -> Result<(), String> {
        let paths = bus::BusPaths {
            nats_dir: &self.paths.data_dir.join("nats"),
            config_path: &self.paths.config_path,
            ck_bus: &self
                .paths
                .binary_home
                .join(components::platform_binary("ck-bus")),
            ck_auth: &self.auth_path(),
        };
        let changed = bus::configure(
            &paths,
            &self
                .paths
                .binary_home
                .join(components::platform_binary("ck-nats-server")),
            &subc_daemon::bootstrap::connection_file_path(),
            &mut self.inventory,
            &mut *self.runner,
            &mut *self.bus_control,
            std::thread::sleep,
        )?;
        if let Some(changed) = changed {
            self.component_steps
                .entry(Component::Core)
                .or_default()
                .message_bus_changed |= changed;
        } else {
            println!("  {}", bus::HAND_MADE);
        }
        Ok(())
    }

    fn observed_core_version(&self) -> Option<String> {
        // The connection file's daemon_ver identifies the subc-daemon library,
        // not the subc-core release that setup's index and module floors name.
        components::observed_binary_version(&self.paths.runtime_paths.daemon)
    }

    /// Returns enabled module ids only when the daemon is live. A stopped daemon
    /// has no observable registry and will load the managed configuration on start.
    fn live_enabled_modules(&self) -> Result<Option<BTreeSet<String>>, String> {
        if !self.runtime_status.live {
            return Ok(None);
        }
        #[cfg(feature = "test-support")]
        if let Some(modules) = env::var_os("CK_TEST_SETUP_MODULES") {
            return Ok(Some(
                modules
                    .to_string_lossy()
                    .split(',')
                    .filter(|module| !module.is_empty())
                    .map(ToOwned::to_owned)
                    .collect(),
            ));
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
        // The installer keeps the operator command in cmd/ while setup owns
        // daemon and module binaries in bin/; both bootstrap locations may
        // contain an unowned ck that setup must adopt.
        let command_home = self.paths.data_dir.join("cmd");
        let command_home = fs::canonicalize(&command_home).unwrap_or(command_home);
        // Resolve the executable the same way as the two folders, so a symlink in
        // the data folder's path (macOS reaches its temp and some home folders
        // through /var -> /private/var) cannot make a bootstrap ck look foreign.
        let executable =
            fs::canonicalize(&self.executable).unwrap_or_else(|_| self.executable.clone());
        let is_bootstrap_ck =
            executable.starts_with(&binary_home) || executable.starts_with(&command_home);
        is_bootstrap_ck
            .then(|| self.executable.clone())
            .filter(|path| path.is_file())
            .filter(|path| !self.inventory.owns_path("managed-binary", path))
    }

    /// The `ckni` beside the running `ck`, read only when that `ck` is one
    /// the manifest owns or this run adopts; setup never puts `ckni` beside
    /// a `ck` it doesn't manage.
    fn ckni_observation(&self, adopting: bool) -> Option<ckni::CkniObservation> {
        (adopting || ckni::ck_is_owned(&self.executable, &self.inventory))
            .then(|| ckni::observe(&self.executable, &self.inventory, ckni::Mechanism::CURRENT))
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
        #[cfg(test)]
        if !self.inventory.transaction_is_open() {
            self.inventory.save()?;
        }
        println!(
            "  adopted {} from the bootstrap installer",
            components::display_home_path(path)
        );
        Ok(())
    }

    /// Whether the daemon reports the claustrum module live right now. A
    /// live claustrum holds the vault's single-writer lease, which is also
    /// the proof its credentials exist: bootstrap is an offline verb that
    /// refuses under the lease, so running it here would fail a setup whose
    /// only defect was a lost ownership record. Unreadable status is "not
    /// live" and bootstrap proceeds as before.
    fn claustrum_module_is_live(&self) -> bool {
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            return false;
        }
        let Ok(output) = Command::new(&self.executable)
            .args(["--json", "module", "status", "claustrum"])
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|value| {
                value
                    .get("module")
                    .and_then(|module| module.get("live"))
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false)
    }

    fn bootstrap_claustrum(&mut self, key_path: Option<&Path>) -> Result<(), String> {
        if self.claustrum_module_is_live() {
            println!("  claustrum credentials already initialized (module is running)");
            return Ok(());
        }
        let auth = self.paths.binary_home.join(if cfg!(windows) {
            "ck-auth.exe"
        } else {
            "ck-auth"
        });
        let directory = BootstrapDirectory::capture(&self.paths.data_dir)?;
        let result = run_auth_bootstrap(&auth, key_path, &self.paths.data_dir, |mut command| {
            let status = command
                .status()
                .map_err(|error| format!("could not run ck auth bootstrap: {error}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("ck auth bootstrap failed with {status}"))
            }
        });
        if let Err(error) = result {
            self.component_steps
                .entry(Component::Claustrum)
                .or_default()
                .failed_bootstrap_directory = Some(directory);
            return Err(error);
        }
        println!("  initialized claustrum credentials");
        Ok(())
    }

    fn tighten_existing_permissions(&self) -> Result<(), String> {
        let config_directory = self
            .paths
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.paths.data_dir.clone());
        for (path, ceiling) in [
            (self.paths.data_dir.clone(), 0o700),
            (self.paths.binary_home.clone(), 0o755),
            (config_directory, 0o700),
            (self.paths.data_dir.join("cmd"), 0o755),
            (self.paths.data_dir.join("claustrum"), 0o700),
            (self.paths.data_dir.join("stores"), 0o700),
        ] {
            file_modes::tighten_owned_directory(&path, ceiling)?;
        }

        if self
            .inventory
            .owns_path("runtime-definition", &self.paths.runtime_paths.definition)
            || self
                .inventory
                .owns_path("runtime-registration", &self.paths.runtime_paths.definition)
        {
            if let Some(parent) = self.paths.runtime_paths.definition.parent() {
                file_modes::tighten_owned_directory(parent, 0o755)?;
            }
        }

        let inventory_path = self.paths.data_dir.join("installer-manifest.json");
        file_modes::tighten_owned_file(&inventory_path, 0o644)?;
        file_modes::tighten_inventory_files(&self.inventory)?;
        if let Some(key_path) = &self.paths.claustrum_key_path {
            file_modes::tighten_owned_file(key_path, 0o600)?;
        }
        Ok(())
    }

    fn permissions_need_tightening(&self) -> Result<bool, String> {
        let config_directory = self
            .paths
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.paths.data_dir.clone());
        for (path, ceiling) in [
            (self.paths.data_dir.clone(), 0o700),
            (self.paths.binary_home.clone(), 0o755),
            (config_directory, 0o700),
            (self.paths.data_dir.join("cmd"), 0o755),
            (self.paths.data_dir.join("claustrum"), 0o700),
            (self.paths.data_dir.join("stores"), 0o700),
        ] {
            if file_modes::directory_needs_tightening(&path, ceiling)? {
                return Ok(true);
            }
        }

        if self
            .inventory
            .owns_path("runtime-definition", &self.paths.runtime_paths.definition)
            || self
                .inventory
                .owns_path("runtime-registration", &self.paths.runtime_paths.definition)
        {
            if let Some(parent) = self.paths.runtime_paths.definition.parent() {
                if file_modes::directory_needs_tightening(parent, 0o755)? {
                    return Ok(true);
                }
            }
        }

        if file_modes::file_needs_tightening(
            &self.paths.data_dir.join("installer-manifest.json"),
            0o644,
        )? {
            return Ok(true);
        }
        for kind in ["managed-binary", "binary-placement"] {
            for path in self.inventory.paths_for_kind(kind) {
                if file_modes::file_needs_tightening(&path, 0o755)? {
                    return Ok(true);
                }
            }
        }
        for path in self.inventory.paths_for_kind("configuration") {
            if file_modes::file_needs_tightening(&path, 0o600)? {
                return Ok(true);
            }
        }
        for kind in ["runtime-definition", "runtime-registration"] {
            for path in self.inventory.paths_for_kind(kind) {
                if file_modes::file_needs_tightening(&path, 0o644)? {
                    return Ok(true);
                }
            }
        }
        if let Some(key_path) = &self.paths.claustrum_key_path {
            if file_modes::file_needs_tightening(key_path, 0o600)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Runs `ck-auth bootstrap` through `run`, which executes the command and
/// reports whether it succeeded. ck-auth refuses a key path whose folder does
/// not exist, so any missing folders on the key path are created first, on
/// every platform. On Unix, folders created here are owner-only (an existing
/// key folder inside the CortexKit data folder is also narrowed to
/// owner-only), and the command runs under umask 077 so the key it writes is
/// private from the start. Windows creates the operator's folders without
/// changing their inherited access permissions.
fn run_auth_bootstrap(
    auth: &Path,
    key_path: Option<&Path>,
    data_dir: &Path,
    run: impl FnOnce(Command) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(key_path) = key_path {
        if let Some(parent) = key_path.parent() {
            if key_path.starts_with(data_dir) {
                file_modes::ensure_owned_directory(parent, 0o700)?;
            } else {
                file_modes::ensure_private_parents(parent)?;
            }
        }
    }
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "umask 077; exec \"$@\"", "ck-auth-bootstrap"])
            .arg(auth);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(auth);
    command.arg("bootstrap");
    if let Some(key_path) = key_path {
        command.arg("--key-path").arg(key_path);
    }
    run(command).map_err(|error| {
        if key_path.is_some() {
            error
        } else {
            format!("{error}\n{}", auth_key_path_hint())
        }
    })?;
    if let Some(key_path) = key_path {
        #[cfg(unix)]
        if !fs::symlink_metadata(key_path).is_ok_and(|metadata| metadata.file_type().is_file()) {
            return Err(format!(
                "claustrum key {} is not a regular file",
                key_path.display()
            ));
        }
        file_modes::tighten_owned_file(key_path, 0o600)?;
    }
    Ok(())
}

fn auth_key_path_hint() -> String {
    #[cfg(not(windows))]
    {
        "hint: for a key file instead of the OS key store, run: install -d -m 700 ~/.config/cortexkit/secrets && ck setup claustrum --key-path ~/.config/cortexkit/secrets/master.key".to_string()
    }
    #[cfg(windows)]
    {
        let config = subc_daemon::daemon_config::default_config_path();
        let secrets = config.parent().unwrap_or(Path::new(".")).join("secrets");
        format!(
            "hint: for a key file instead of the OS key store, run in PowerShell: New-Item -ItemType Directory -Force \"{0}\" | Out-Null; ck setup claustrum --key-path \"{1}\"",
            secrets.display(),
            secrets.join("master.key").display()
        )
    }
}

impl SetupExecutor for SetupBackend {
    type Error = String;

    fn prepare_apply(&mut self, plan: &SetupPlan) -> Result<(), Self::Error> {
        if plan
            .operations
            .iter()
            .any(|operation| matches!(operation, SetupOperation::RemoveManagedComponent { .. }))
        {
            return Ok(());
        }
        self.tighten_existing_permissions()?;
        let has_mutating_operation = plan.operations.iter().any(|op| {
            op.mutates()
                && !matches!(
                    op,
                    SetupOperation::GrantMessageKeys
                        | SetupOperation::ConfigureMessageBus
                        | SetupOperation::ConfigureMessageHub
                )
        });
        if plan.permission_repair && !has_mutating_operation {
            println!("  tightened permissions on existing CortexKit paths");
        }
        Ok(())
    }

    fn apply(&mut self, operation: &SetupOperation) -> Result<(), Self::Error> {
        match operation {
            SetupOperation::InstallComponent { component } => {
                let release = self.artifacts.release_summary(*component)?;
                println!(
                    "Installing {} ({}, {})",
                    component.module_id().unwrap_or("CortexKit"),
                    release.release,
                    release.target
                );
                components::install_component(
                    *component,
                    &self.paths.binary_home,
                    &mut self.inventory,
                    &mut self.artifacts,
                )
            }
            SetupOperation::ConfigureComponent { component } => {
                let mut nats = if *component == Component::Core {
                    self.legacy_nats_migration()?
                } else {
                    None
                };
                components::configure_component(
                    *component,
                    &self.paths.config_path,
                    &self.paths.binary_home,
                    self.paths.claustrum_key_path.as_deref(),
                    &mut self.inventory,
                )?;
                if let Some(migration) = nats.as_mut() {
                    migration.apply_recorded(&mut self.inventory)?;
                    self.component_steps
                        .entry(*component)
                        .or_default()
                        .nats_migration = nats;
                }
                Ok(())
            }
            SetupOperation::BootstrapClaustrum { key_path } => {
                self.bootstrap_claustrum(key_path.as_deref())
            }
            operation @ (SetupOperation::WriteModuleSettings { .. }
            | SetupOperation::EnsureBrocaKey { .. }
            | SetupOperation::GrantComponentVault { .. }
            | SetupOperation::RegisterCerebellumNativeHost
            | SetupOperation::EnsureCerebellumExtension { .. }
            | SetupOperation::EnsurePlexusCatalog { .. }) => {
                let result = self.apply_module_step(operation);
                if result.is_err() {
                    let component = operation.component().expect("module ensure component");
                    self.component_steps
                        .entry(component)
                        .or_default()
                        .module_step_failed = true;
                }
                result
            }
            SetupOperation::EnablePlexusAgentPolicies => {
                let admin = self
                    .paths
                    .binary_home
                    .join(components::platform_binary("ck-plexus-admin"));
                let changed =
                    plexus_policy::offer(&admin, &subc_daemon::bootstrap::connection_file_path());
                self.component_steps
                    .entry(Component::Plexus)
                    .or_default()
                    .plexus_maintenance_changed |= changed;
                Ok(())
            }
            SetupOperation::GrantMessageKeys => {
                println!("{}", super::message_plane::GRANT_STEP);
                let auth = self.auth_path();
                if !owns_binary(&self.inventory, &auth) {
                    return Err(super::text::unowned_binary_refusal(&auth));
                }
                super::message_plane::wait_for_vault(
                    &self.executable,
                    None,
                    &mut *self.runner,
                    super::upgrade_executor::RESTART_COMPLETION_TIMEOUT,
                )?;
                let (changed, detail) = super::message_plane::grant_keys(&auth, &mut *self.runner)?;
                self.component_steps
                    .entry(Component::Claustrum)
                    .or_default()
                    .message_grants_changed |= changed;
                println!("  {detail}");
                Ok(())
            }
            SetupOperation::ConfigureMessageBus => self.configure_message_bus(),
            SetupOperation::ConfigureMessageHub => {
                println!("{}", super::message_plane::HUB_STEP);
                if let Some(change) = super::message_plane::plan_hub(
                    &self.paths.config_path,
                    &self.paths.binary_home,
                    &self.paths.data_dir,
                    &subc_daemon::bootstrap::connection_file_path(),
                )? {
                    config::apply_recorded(&change, &mut self.inventory)?;
                    self.component_steps
                        .entry(Component::Core)
                        .or_default()
                        .message_hub_change = Some(change);
                    println!("  added idle nats-hub entry");
                } else {
                    println!("  Message hub entry: nothing changed (existing entry kept)");
                }
                Ok(())
            }
            SetupOperation::AdoptRunningCk { path } => self.adopt_running_ck(path),
            SetupOperation::LinkCkni { ck, .. } => {
                if let Some(detail) = ckni::install(
                    ck,
                    &mut self.inventory,
                    ckni::Mechanism::CURRENT,
                    super::text::unowned_binary_refusal,
                )? {
                    println!("  {detail}");
                }
                Ok(())
            }
            SetupOperation::RescanComponent { .. } => self.run_ck(&["module", "rescan"]),
            SetupOperation::EnableComponent { component } => {
                self.enable_transactional_module(*component)
            }
            SetupOperation::RestartRuntime { .. } => {
                self.write_runtime_transaction(false)?;
                println!("  restarted the daemon");
                Ok(())
            }
            SetupOperation::RegisterRuntime => {
                self.write_runtime_transaction(true)?;
                println!("  registered and started the daemon");
                Ok(())
            }
            SetupOperation::UpdateRuntimeDefinition { restart } => {
                if self.platform == RuntimePlatform::Macos
                    && !restart
                    && runtime::observe(self.platform, &mut self.runner)?.live
                {
                    return Err("refusal: daemon became live after planning; re-run ck setup to confirm a LaunchAgent reload".into());
                }
                self.write_runtime_transaction(true)?;
                println!("  updated the daemon service definition");
                Ok(())
            }
            // separate operation remains in the plan so its current-liveness
            // requirement is visible before execution.
            SetupOperation::StartRuntime => Ok(()),
            SetupOperation::MonitorNatsServer { .. } => Ok(()),
            SetupOperation::DeregisterRuntime => Ok(()),
            SetupOperation::RemoveManagedComponent { .. } if self.uninstall_report.is_none() => {
                // The daemon's own connection file names the pid to stop on a
                // platform whose task deletion does not stop it; absent or
                // unreadable means no daemon to stop.
                let daemon_pid = subc_transport::connection_file::discover(None)
                    .ok()
                    .map(|found| found.info.pid);
                let report = uninstall::uninstall(
                    self.platform,
                    &self.paths.runtime_paths,
                    daemon_pid,
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
            | SetupOperation::OfferConversion { .. }
            | SetupOperation::ConfirmConversion { .. }
            | SetupOperation::Validate { .. }
            | SetupOperation::RetainUserData => Ok(()),
        }
    }

    fn runtime_was_live(&self) -> bool {
        self.runtime_status.live
    }

    fn preview_restart_required(&mut self) -> Result<Vec<String>, Self::Error> {
        // The faked-live runtime has no daemon to ask; the sections come from
        // the test instead, empty unless it names some.
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            return Ok(env::var("CK_TEST_SETUP_RESTART_REQUIRED")
                .ok()
                .map(|sections| {
                    sections
                        .split(',')
                        .filter(|section| !section.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default());
        }
        let preview = self
            .rescan_preview
            .ok_or_else(|| "no authenticated rescan preview client was supplied".to_string())?;
        preview()
    }
}

fn owns_binary(inventory: &Inventory, path: &Path) -> bool {
    components::OWNED_BINARY_KINDS
        .iter()
        .any(|kind| inventory.owns_path(kind, path))
}

struct OwnedBusMonitor<'a>(&'a Inventory);

impl bus_monitoring::ConfTool for OwnedBusMonitor<'_> {
    fn conf_only(
        &mut self,
        target: &bus_monitoring::BusTarget,
        dry_run: bool,
    ) -> Result<bus_monitoring::ConfReport, String> {
        check_monitor_ownership(self.0, target)?;
        bus_monitoring::CkBusCommand.conf_only(target, dry_run)
    }
}

fn check_monitor_ownership(
    inventory: &Inventory,
    target: &bus_monitoring::BusTarget,
) -> Result<(), String> {
    if !owns_binary(inventory, &target.ckbus) {
        return Err(format!(
            "{} is not owned by setup; it was not run",
            target.ckbus.display()
        ));
    }
    Ok(())
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
    fn run(
        &mut self,
        label: &str,
        args: &[String],
    ) -> Result<validation::ValidationResult, String> {
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            return Ok(validation::ValidationResult::success());
        }
        let output = Command::new(self.executable)
            .args(args)
            .output()
            .map_err(|error| format!("could not run {label} {}: {error}", args.join(" ")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let captured = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout,
            (true, false) => stderr,
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        Ok(if output.status.success() {
            validation::ValidationResult::success()
        } else {
            validation::ValidationResult::failed(captured)
        })
    }
}

pub fn default_claustrum_key_path() -> Result<Option<PathBuf>, String> {
    Ok(None)
}

/// The installed daemon service as the service manager sees it, for the
/// app's daemon-down screen.
pub fn installed_service_facts() -> Result<runtime::ServiceFacts, String> {
    let platform = RuntimePlatform::current();
    let paths =
        runtime::session_runtime_paths(platform, &data_directory()?.join("bin"), &user_home()?);
    Ok(runtime::service_facts(
        platform,
        &paths,
        &mut SystemCommandRunner,
    ))
}

/// Starts the installed daemon service through the service manager, the
/// way setup starts it, without changing the installed definition.
pub fn start_installed_service() -> Result<(), String> {
    let platform = RuntimePlatform::current();
    let paths =
        runtime::session_runtime_paths(platform, &data_directory()?.join("bin"), &user_home()?);
    runtime::start_installed(platform, &paths, &mut SystemCommandRunner)
}

pub fn data_directory() -> Result<PathBuf, String> {
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

pub(crate) fn user_home() -> Result<PathBuf, String> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "the user home directory is unavailable for setup".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    include!("apply/nats_tests.rs");

    #[cfg(unix)]
    #[test]
    fn legacy_monitoring_probes_only_an_inventory_owned_ck_bus() {
        let root = ScratchDir::new("setup-legacy-monitoring-owner");
        let config = root.join("subc.jsonc");
        let server_conf = root.join("server.conf");
        let bus = root.join("external-ck-bus");
        let sentinel = root.join("monitor-probe-ran");
        fs::write(&server_conf, b"operator-owned NATS configuration").unwrap();
        let before = serde_json::json!({"modules": {
            "nats-server": {"protocol": "none", "args": ["-c", server_conf]},
            "ckbus": {"program": bus},
        }})
        .to_string();
        fs::write(&config, &before).unwrap();
        super::super::test_exec::write_executable(&bus, format!(
            "#!/bin/sh\ntouch '{}'\nprintf '%s\\n' '{{\"status\":\"would apply\",\"health_url\":\"http://127.0.0.1:18222/healthz\"}}'\n",
            sentinel.display()
        ).as_bytes());
        let mut inventory = Inventory::load(root.join("inventory.json"), "linux-x64").unwrap();

        let observed = bus_monitoring::observe(&config, &mut OwnedBusMonitor(&inventory));
        assert!(
            matches!(observed, BusMonitoring::Skipped { reason } if reason.contains("not owned by setup; it was not run"))
        );
        assert!(!sentinel.exists());

        inventory.record("binary-placement", &bus, Map::new());
        let observed = bus_monitoring::observe(&config, &mut OwnedBusMonitor(&inventory));
        assert!(matches!(observed, BusMonitoring::Observed(target) if target.needs_change()));
        assert!(sentinel.is_file());
        assert_eq!(fs::read_to_string(config).unwrap(), before);
        assert_eq!(
            fs::read(server_conf).unwrap(),
            b"operator-owned NATS configuration"
        );
    }

    struct UnavailableUserManager;

    impl CommandRunner for UnavailableUserManager {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            Ok(runtime::CommandResult {
                success: program == "systemctl" && args == ["show-environment"],
                exit_code: Some(if program == "systemctl" && args == ["show-environment"] {
                    0
                } else {
                    1
                }),
                stdout: String::new(),
                stderr: format!("{program} {}: user manager unavailable", args.join(" ")),
            })
        }
    }
    #[cfg(unix)]
    #[test]
    fn failed_bootstrap_rolls_back_only_a_new_vault_directory() {
        use super::super::test_exec::write_executable;
        for existed_before in [false, true] {
            let root = ScratchDir::new("setup-bootstrap-rollback");
            let mut backend = backend_without_user_manager(&root);
            let vault = backend.paths.data_dir.join("claustrum");
            if existed_before {
                fs::create_dir_all(vault.join("nested")).unwrap();
                fs::write(vault.join("store.db"), b"existing encrypted vault").unwrap();
                fs::write(vault.join("nested/operator-file"), b"keep me").unwrap();
            }
            fs::create_dir_all(&backend.paths.binary_home).unwrap();
            let quoted_vault = vault.to_string_lossy().replace('\'', "'\\''");
            write_executable(
                &backend.paths.binary_home.join("ck-auth"),
                format!("#!/bin/sh\nmkdir -p '{quoted_vault}'\ntouch '{quoted_vault}/store.db'\necho 'keyring refused; use --key-path' >&2\nexit 4\n").as_bytes(),
            );

            let error = backend.bootstrap_claustrum(None).unwrap_err();
            assert!(error.contains("hint:"), "{error}");
            assert!(vault.join("store.db").exists());
            let snapshot = backend.component_steps[&Component::Claustrum]
                .failed_bootstrap_directory
                .as_ref()
                .unwrap();
            assert_eq!(snapshot.existed_before, existed_before);
            backend
                .component_steps
                .get_mut(&Component::Claustrum)
                .unwrap()
                .failed_bootstrap_directory
                .take()
                .unwrap()
                .rollback()
                .unwrap();
            if existed_before {
                assert_eq!(
                    fs::read(vault.join("store.db")).unwrap(),
                    b"existing encrypted vault"
                );
                assert_eq!(
                    fs::read(vault.join("nested/operator-file")).unwrap(),
                    b"keep me"
                );
            } else {
                assert!(
                    !vault.exists(),
                    "failed bootstrap must not leave a partial vault"
                );
            }
        }
    }

    #[test]
    fn bootstrap_rollback_names_the_removed_or_preserved_directory() {
        let root = ScratchDir::new("setup-bootstrap-rollback-message");
        let directory = BootstrapDirectory::capture(root.path()).unwrap();
        fs::create_dir(&directory.path).unwrap();
        let path = directory.path.clone();
        assert_eq!(
            directory.rollback().unwrap(),
            format!("rolled back: {}", path.display())
        );
        fs::create_dir(&path).unwrap();
        let directory = BootstrapDirectory::capture(root.path()).unwrap();
        assert_eq!(
            directory.rollback().unwrap(),
            format!(
                "setup left existing vault directory in place: {}",
                path.display()
            )
        );
        assert!(path.exists());
    }

    #[test]
    fn auth_bootstrap_default_passes_no_key_path_and_keeps_the_refusal_with_a_hint() {
        let root = ScratchDir::new("setup-auth-default");
        let error = run_auth_bootstrap(
            &root.join("ck-auth"),
            default_claustrum_key_path().unwrap().as_deref(),
            root.path(),
            |command| {
                assert!(!command.get_args().any(|arg| arg == "--key-path"));
                Err("Secret Service unavailable; use --key-path".to_string())
            },
        )
        .unwrap_err();
        assert!(error.starts_with("Secret Service unavailable; use --key-path\n"));
        assert!(error.contains(&auth_key_path_hint()), "{error}");
        assert!(!root.join("claustrum/master.key").exists());
    }

    #[test]
    fn auth_bootstrap_explicit_key_failure_does_not_suggest_a_different_path() {
        let root = ScratchDir::new("setup-auth-explicit-failure");
        let error = run_auth_bootstrap(
            &root.join("ck-auth"),
            Some(&root.join("secrets/master.key")),
            &root.join("data"),
            |_| Err("operator key refused".to_string()),
        )
        .unwrap_err();
        assert_eq!(error, "operator key refused");
    }

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

    #[test]
    fn setup_refuses_without_writing_when_the_user_manager_is_unavailable() {
        let root = ScratchDir::new("setup-user-manager-preflight");
        let mut backend = backend_without_user_manager(&root);

        let error = backend
            .observe(&SetupRequest::install(Vec::new()))
            .unwrap_err();

        assert!(
            error.contains("systemd-user-manager-unavailable"),
            "{error}"
        );
        assert!(error.contains("loginctl enable-linger $USER"), "{error}");
        assert!(
            !root.join("installer-manifest.json").exists(),
            "setup must not save its inventory"
        );
        assert!(
            !backend.paths.binary_home.exists(),
            "setup must not create the binary directory"
        );
        assert!(
            !backend.paths.config_path.exists(),
            "setup must not write configuration"
        );
        assert!(
            !backend.paths.runtime_paths.definition.exists(),
            "setup must not write a service definition"
        );
    }

    #[test]
    fn setup_uninstall_does_not_refuse_when_the_user_manager_is_unavailable() {
        let root = ScratchDir::new("setup-uninstall-without-user-manager");
        let mut backend = backend_without_user_manager(&root);
        let mut request = SetupRequest::install(Vec::new());
        request.uninstall = true;

        if let Err(error) = backend.observe(&request) {
            assert!(
                !error.contains("systemd-user-manager-unavailable"),
                "{error}"
            );
            assert!(!error.contains("systemd-unavailable"), "{error}");
        }
    }

    pub(super) fn backend_without_user_manager(root: &ScratchDir) -> SetupBackend {
        let binary_home = root.join("bin");
        let inventory_path = root.join("installer-manifest.json");
        let platform = RuntimePlatform::Linux;
        let inventory = Inventory::load(&inventory_path, "linux-x64").unwrap();
        SetupBackend {
            bus_control: crate::setup::bus_control(&binary_home.join("ck")),
            executable: binary_home.join("ck"),
            paths: SetupPaths {
                data_dir: root.join("data"),
                binary_home: binary_home.clone(),
                config_path: root.join("subc.jsonc"),
                claustrum_key_path: None,
                runtime_paths: runtime::runtime_paths(platform, &binary_home, root.path()),
            },
            platform,
            inventory,
            runner: Box::new(UnavailableUserManager),
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
            rescan_preview: None,
        }
    }

    #[test]
    fn rescan_preview_uses_the_injected_client_and_preserves_its_result() {
        let root = ScratchDir::new("setup-injected-preview");
        let mut backend = backend_without_user_manager(&root);
        assert_eq!(
            backend.preview_restart_required().unwrap_err(),
            "no authenticated rescan preview client was supplied"
        );
        backend.set_rescan_preview(|| Ok(vec!["modules".into()]));
        assert_eq!(backend.preview_restart_required().unwrap(), ["modules"]);
        backend.set_rescan_preview(|| Err("preview refused".into()));
        assert_eq!(
            backend.preview_restart_required().unwrap_err(),
            "preview refused"
        );
    }

    /// ck-auth refuses a key path whose directory is missing, so the directory
    /// must exist by the time the command runs, wherever the operator put it.
    #[test]
    fn auth_bootstrap_creates_the_key_directory_before_ck_auth_runs() {
        let root = ScratchDir::new("setup-auth-key-directory");
        let data_dir = root.join("cortexkit");
        fs::create_dir(&data_dir).unwrap();
        let auth = data_dir.join("bin").join("ck-auth");
        for key_path in [
            data_dir.join("operator-secrets").join("master.key"),
            root.join("custom").join("keys").join("master.key"),
        ] {
            let mut directory_existed = None;
            let result = run_auth_bootstrap(&auth, Some(&key_path), &data_dir, |command| {
                let args = command.get_args().collect::<Vec<_>>();
                let flag = args
                    .iter()
                    .position(|arg| *arg == "--key-path")
                    .expect("the command names the key path");
                let commanded = Path::new(args[flag + 1]);
                directory_existed = Some(commanded.parent().unwrap().is_dir());
                // Stand in for ck-auth: write the key it was asked for, and
                // refuse a key path whose directory is missing, as it does.
                fs::write(commanded, "secret").map_err(|error| error.to_string())
            });
            assert_eq!(
                directory_existed,
                Some(true),
                "{} must exist when ck-auth runs",
                key_path.parent().unwrap().display()
            );
            result.unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(data_dir.join("operator-secrets")), 0o700);
            assert_eq!(mode(root.join("custom")), 0o700);
            assert_eq!(mode(root.join("custom").join("keys")), 0o700);
        }
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
#[path = "apply/real_auth_tests.rs"]
mod real_auth_tests;

#[cfg(all(test, unix))]
mod adoption_tests {
    use super::*;
    use crate::setup::test_exec::write_executable;
    use cortexkit_test_support::ScratchDir;

    /// Rollback after a later refusal must delete only what this run wrote.
    #[test]
    fn bootstrap_placed_ck_is_adopted_as_managed_binary() {
        let root = ScratchDir::new("setup-adopt-running-ck");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).unwrap();
        let placed = binary_home.join("ck");
        write_executable(&placed, b"#!/bin/sh\necho 'ck 0.16.2'\n");
        let executable = fs::canonicalize(&placed).unwrap();
        let platform = RuntimePlatform::current();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        inventory.record("binary-placement", &executable, Map::new());
        let mut backend = SetupBackend {
            bus_control: crate::setup::bus_control(&executable),
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
            runner: Box::new(SystemCommandRunner),
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
            rescan_preview: None,
        };

        backend.adopt_running_ck(&executable).unwrap();

        assert!(backend.inventory.owns_path("managed-binary", &executable));
        assert!(
            !backend.inventory.owns_path("binary-placement", &executable),
            "adoption transfers the row; two ownership rows for one path let a stale digest win later"
        );
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
        let root = ScratchDir::new("setup-adopt-archive-digest");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).unwrap();
        let placed = binary_home.join("ck");
        write_executable(&placed, b"#!/bin/sh\necho 'ck 0.16.2'\n");
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
            bus_control: crate::setup::bus_control(&executable),
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
            runner: Box::new(SystemCommandRunner),
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
            rescan_preview: None,
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
