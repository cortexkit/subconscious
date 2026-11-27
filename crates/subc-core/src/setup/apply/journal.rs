use super::*;
use crate::setup::install_journal::{activation, files, system::SystemRecovery, Build};
use crate::setup::planner::{execute_setup_with_prepare, ExecutionMode};

impl SetupBackend {
    #[doc(hidden)]
    pub fn pause_confirmation(
        &mut self,
        plan: &SetupPlan,
        request: &SetupRequest,
    ) -> Result<crate::setup::prompt::PromptSnapshot, String> {
        let snapshot = crate::setup::prompt::PromptSnapshot::capture(
            self.inventory.manifest(),
            [
                self.paths.config_path.clone(),
                self.paths.runtime_paths.definition.clone(),
            ],
        )?;
        self.print_dry_run(plan)?;
        self.print_proposed_diffs(plan, request)?;
        self.inventory.release_lock();
        Ok(snapshot)
    }

    #[doc(hidden)]
    pub fn resume_confirmation(
        &mut self,
        snapshot: crate::setup::prompt::PromptSnapshot,
        request: &SetupRequest,
        observed: &SetupObserved,
        plan: &SetupPlan,
    ) -> Result<(), String> {
        let manifest = self.inventory.manifest().to_owned();
        let _hold = crate::setup::install_lock::InstallHold::take(
            &manifest,
            "ck setup",
            crate::setup::install_lock::WAIT,
        )?;
        snapshot.check()?;
        self.inventory = Inventory::load_locked(
            manifest,
            &PlatformObservation::current().to_string(),
            "ck setup",
            crate::setup::install_lock::WAIT,
        )?;
        self.artifacts = ReleaseArtifactSource::current();
        let current = self.observe(request)?;
        if !matches_confirmed_setup(observed, &current, request, plan) {
            return Err("refusal: the setup plan changed while waiting for confirmation; rerun ck setup to review the new plan".into());
        }
        Ok(())
    }

    pub(super) fn execute_transactions(
        &mut self,
        plan: &SetupPlan,
        uninstall: bool,
    ) -> Result<(), String> {
        let mut groups = BTreeMap::<Component, Vec<SetupOperation>>::new();
        let mut grants = false;
        let mut plexus_policies = false;
        let mut monitor = None;
        for operation in plan
            .operations
            .iter()
            .filter(|operation| operation.mutates())
        {
            if matches!(operation, SetupOperation::GrantMessageKeys) {
                grants = true;
                continue;
            }
            if matches!(operation, SetupOperation::EnablePlexusAgentPolicies) {
                plexus_policies = true;
                continue;
            }
            if let SetupOperation::MonitorNatsServer { target } = operation {
                monitor = Some(target);
                continue;
            }
            let component = if uninstall {
                Component::Core
            } else {
                operation.component().unwrap_or(Component::Core)
            };
            groups.entry(component).or_default().push(operation.clone());
        }
        self.prepare_apply(plan)?;
        let mut unchanged = Vec::new();
        for component in std::iter::once(Component::Core).chain(
            Component::ALL
                .into_iter()
                .filter(|&component| component != Component::Core),
        ) {
            let Some(operations) = groups.remove(&component) else {
                if plan.validation.contains(&component) {
                    unchanged.push(component);
                }
                continue;
            };
            let fusiform_is_starting = component == Component::Fusiform
                && operations.iter().any(|operation| {
                    matches!(
                        operation,
                        SetupOperation::InstallComponent { .. }
                            | SetupOperation::EnableComponent { .. }
                    )
                });
            if self.platform == RuntimePlatform::Macos
                && operations.iter().any(|operation| {
                    matches!(
                        operation,
                        SetupOperation::RegisterRuntime
                            | SetupOperation::UpdateRuntimeDefinition { .. }
                            | SetupOperation::DeregisterRuntime
                            | SetupOperation::RestartRuntime { .. }
                    )
                })
            {
                let status = runtime::observe(self.platform, &mut self.runner)?;
                super::super::install_journal::registration::refuse_stopped_launch_agent(
                    self.platform,
                    status,
                )?;
            }
            let subplan = SetupPlan {
                operations,
                outcomes: Vec::new(),
                validation: Vec::new(),
                permission_repair: false,
            };
            self.inventory.start_transaction()?;
            // Vault credential initialization is idempotent vault-owned data.
            // A fresh vault must initialize before it can start; the journal
            // never stores a key or rolls back successfully created vault data.
            let applied = (|| {
                if uninstall {
                    self.stop_before_uninstall()?;
                }
                execute_setup_with_prepare(&subplan, ExecutionMode::Apply, self, false)?;
                self.verify_transaction(
                    component,
                    uninstall,
                    plan.validation.contains(&component),
                    fusiform_is_starting,
                )
            })();
            let mut recovery = SystemRecovery {
                executable: &self.executable,
                connection: None,
                platform: self.platform,
                paths: &self.paths.runtime_paths,
                runner: &mut self.runner,
            };
            if let Err(error) = applied {
                if let Err(undo) = self.inventory.undo_transaction(&mut recovery) {
                    return Err(format!("{error}; install recovery stopped: {undo}"));
                }
                let steps = self.component_steps.entry(component).or_default();
                if let Some(directory) = steps.failed_bootstrap_directory.take() {
                    crate::cli_output::println!("{}", directory.rollback()?);
                }
                if std::mem::take(&mut steps.module_step_failed) {
                    return Err(super::module_steps::refusal_remedy(plan, component, &error));
                }
                return Err(error);
            }
            self.inventory.commit_transaction(&mut recovery)?;
        }
        if grants {
            // The vault owns idempotence for its grants; make them only once
            // the installation whose keys will be used has been committed.
            self.apply(&SetupOperation::GrantMessageKeys)?;
        }
        if plexus_policies {
            // The operator owns this change; do not ask until Plexus is committed.
            self.apply(&SetupOperation::EnablePlexusAgentPolicies)?;
        }
        if let Some(target) = monitor {
            #[cfg(feature = "test-support")]
            super::super::install_journal::crash_hook("before-bus-monitoring");
            self.monitor_nats_after_commit(target, &mut bus_monitoring::CkBusCommand)?;
        }
        if !unchanged.is_empty() {
            let mut validator = CkValidator {
                executable: &self.executable,
            };
            validation::validate_selected(&mut validator, &unchanged, false)?;
        }
        Ok(())
    }

    fn monitor_nats_after_commit(
        &mut self,
        target: &bus_monitoring::BusTarget,
        tool: &mut impl bus_monitoring::ConfTool,
    ) -> Result<(), String> {
        // ck-bus owns the listener file. Its idempotent edit runs after setup
        // commits; setup journals only its own health entry.
        let report = bus_monitoring::apply_listener(target, tool)?;
        self.inventory.start_transaction()?;
        let result =
            bus_monitoring::apply_health(&self.paths.config_path, target, report, |change| {
                config::apply_recorded(change, &mut self.inventory)
            });
        let mut recovery = super::super::install_journal::bootstrap::FileRecovery;
        match result {
            Ok(lines) => {
                self.inventory.commit_transaction(&mut recovery)?;
                for line in lines {
                    crate::cli_output::println!("  {line}");
                }
                Ok(())
            }
            Err(error) => {
                self.inventory
                    .undo_transaction(&mut recovery)
                    .map_err(|undo| format!("{error}; install recovery stopped: {undo}"))?;
                Err(error)
            }
        }
    }

    fn stop_before_uninstall(&mut self) -> Result<(), String> {
        let paths = &self.paths.runtime_paths;
        if !self
            .inventory
            .owns_path("runtime-registration", &paths.definition)
        {
            return Ok(());
        }
        let target = &paths.daemon;
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: None,
            platform: self.platform,
            paths,
            runner: &mut self.runner,
        };
        let mut control = recovery.control(target, None);
        let previous = activation::capture(&mut control, None)?;
        if previous.live.is_none() {
            return Ok(());
        }
        let transaction = self
            .inventory
            .transaction()
            .ok_or("uninstall has no install transaction")?;
        let index =
            activation::prepare_with_before(transaction.journal(), target, None, previous, None)?;
        let step = transaction.journal().step(index);
        activation::replay(step, &mut control)?;
        activation::wait_verified(step, &mut control)?;
        transaction.journal().done(index)
    }

    fn verify_transaction(
        &mut self,
        component: Component,
        uninstall: bool,
        validate_health: bool,
        fusiform_is_starting: bool,
    ) -> Result<(), String> {
        if uninstall {
            return Ok(());
        }
        let mut validator = CkValidator {
            executable: &self.executable,
        };
        if validate_health {
            validation::validate_selected(&mut validator, &[component], fusiform_is_starting)?;
        }
        #[cfg(target_os = "macos")]
        if component == Component::Core && self.paths.runtime_paths.daemon.is_file() {
            let flat = self.paths.binary_home.join("ck-subc");
            for kind in components::OWNED_BINARY_KINDS {
                if let Some(row) = self.inventory.entry_for_path(kind, &flat) {
                    if let Some(digest) = files::digest(&flat)? {
                        if row.get("sha256").and_then(Value::as_str) != Some(&digest) {
                            return Err(format!(
                                "refusal: legacy daemon {} differs from the inventory",
                                flat.display()
                            ));
                        }
                        self.inventory
                            .transaction()
                            .expect("setup transaction")
                            .remove(&flat)?;
                    }
                    self.inventory.remove_owned_path(kind, &flat);
                }
            }
        }
        if let Some(migration) = self
            .component_steps
            .entry(component)
            .or_default()
            .nats_migration
            .as_mut()
        {
            migration.verify(
                &self.executable,
                &mut self.runner,
                super::super::upgrade_executor::RESTART_COMPLETION_TIMEOUT,
            )?;
            migration.complete(&mut self.inventory)?;
        }
        Ok(())
    }

    pub(super) fn write_runtime_transaction(
        &mut self,
        replace_definition: bool,
    ) -> Result<(), String> {
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            if replace_definition {
                let paths = &self.paths.runtime_paths;
                let desired = runtime::desired_definition_checked(self.platform, paths)?;
                self.inventory
                    .transaction()
                    .expect("runtime transaction")
                    .write(&paths.definition, desired.as_bytes(), 0o644)?;
                self.inventory.record(
                    "runtime-definition",
                    &paths.definition,
                    Map::from_iter([(
                        "definition_semantics".into(),
                        Value::String(super::super::definition::canonical(
                            self.platform,
                            &desired,
                        )?),
                    )]),
                );
                self.inventory.record(
                    "runtime-registration",
                    &paths.definition,
                    Map::from_iter([(
                        "identifier".into(),
                        Value::String(self.platform.identifier().into()),
                    )]),
                );
            }
            return Ok(());
        }
        let paths = &self.paths.runtime_paths;
        let target = &paths.daemon;
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: None,
            platform: self.platform,
            paths,
            runner: &mut self.runner,
        };
        let transaction = self
            .inventory
            .transaction()
            .ok_or("runtime change has no install transaction")?;
        let build = activation::installed_daemon_build(target)?;
        let index = activation::prepare(
            transaction.journal(),
            target,
            None,
            Some(build),
            &mut recovery.control(target, None),
        )?;
        let observed = runtime::observe(self.platform, recovery.runner)?;
        if replace_definition {
            recovery.service().install(&mut self.inventory)?;
        }
        let transaction = self
            .inventory
            .transaction()
            .expect("transaction still open after registration");
        let step = transaction.journal().step(index);
        let mut control = recovery.control(target, None);
        if !activation::verify(step, &mut control).unwrap_or(false) {
            if observed.live || observed.registered && !replace_definition {
                activation::Control::restart(&mut control, None)?;
            } else {
                activation::Control::start(&mut control, None)?;
            }
        }
        activation::wait_verified(step, &mut control)?;
        transaction.journal().done(index)
    }

    pub(super) fn enable_transactional_module(
        &mut self,
        component: Component,
    ) -> Result<(), String> {
        #[cfg(feature = "test-support")]
        if env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
            return Ok(());
        }
        let module = component
            .module_id()
            .expect("only modules are enabled by setup");
        let target = components::component_binary_paths(component, &self.paths.binary_home)
            .into_iter()
            .next()
            .ok_or_else(|| format!("no installed program for {module}"))?;
        let build = Build::Image {
            sha256: files::digest(&target)?
                .ok_or_else(|| format!("module executable {} is missing", target.display()))?,
        };
        let paths = &self.paths.runtime_paths;
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: None,
            platform: self.platform,
            paths,
            runner: &mut self.runner,
        };
        let mut control = recovery.control(&target, None);
        let prior = activation::capture(&mut control, Some(module))?;
        if prior.live.as_ref().is_some_and(|live| live.build == build) {
            return Ok(());
        }
        let transaction = self
            .inventory
            .transaction()
            .ok_or("module start has no install transaction")?;
        let index = activation::prepare_with_before(
            transaction.journal(),
            &target,
            Some(module),
            prior,
            Some(build),
        )?;
        let step = transaction.journal().step(index);
        activation::replay(step, &mut control)?;
        activation::wait_verified(step, &mut control)?;
        let reported_build = control.reported_module_build(module);
        transaction.journal().done_with_build(index, reported_build)
    }
}

fn matches_confirmed_setup(
    prior: &SetupObserved,
    current: &SetupObserved,
    request: &SetupRequest,
    plan: &SetupPlan,
) -> bool {
    let proposed = crate::setup::plan_setup(current, request);
    stable_observation(prior.clone()) == stable_observation(current.clone())
        && proposed.is_authorized() == plan.is_authorized()
        && proposed.operations == plan.operations
        && proposed.permission_repair == plan.permission_repair
}

fn stable_observation(mut observed: SetupObserved) -> SetupObserved {
    // The choice to treat a release as unavailable matters; the wording of a
    // network failure does not change which targets the user approved.
    for release in observed.releases.values_mut() {
        if let ReleaseAvailability::Unresolvable { reason } = release {
            reason.clear();
        }
    }
    if let bus_monitoring::BusMonitoring::Skipped { reason } = &mut observed.bus_monitoring {
        reason.clear();
    }
    observed.restart_required.sort_unstable();
    observed.restart_required.dedup();
    observed
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn setup_confirmation_ignores_transient_release_text_but_not_target_changes() {
        let mut prior = SetupObserved::unconfigured_current_host();
        prior.releases.insert(
            Component::Aft,
            ReleaseAvailability::Unresolvable {
                reason: "rate limited".into(),
            },
        );
        let request = SetupRequest::install(vec![]);
        let plan = crate::setup::plan_setup(&prior, &request);
        let mut current = prior.clone();
        current.releases.insert(
            Component::Aft,
            ReleaseAvailability::Unresolvable {
                reason: "request timed out".into(),
            },
        );
        assert!(matches_confirmed_setup(&prior, &current, &request, &plan));
        current
            .components
            .insert(Component::Core, ComponentState::Correct);
        assert!(!matches_confirmed_setup(&prior, &current, &request, &plan));
    }
}
