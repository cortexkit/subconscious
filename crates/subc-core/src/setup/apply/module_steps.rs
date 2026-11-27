use super::super::release_index::ReleaseIndex;
use super::{broca, components, extension, settings, vault, SetupBackend};
use crate::{
    cli_output::println,
    setup::{
        model::{Component, ComponentState, SetupObserved, SetupOperation, SetupRequest},
        planner::SetupPlan,
        runtime::RuntimePlatform,
    },
};
use serde_json::Value;
use std::path::PathBuf;

const NATIVE_HOST: &str = "ck-cerebellum-native-host";
const EXTENSION_UPGRADE: &str = "run ck upgrade, then ck setup, to refresh the Chrome extension";

impl SetupBackend {
    fn auth_program(&self) -> PathBuf {
        self.paths
            .binary_home
            .join(if self.platform == RuntimePlatform::Windows {
                "ck-auth.exe"
            } else {
                "ck-auth"
            })
    }

    pub(super) fn observe_module_steps(
        &mut self,
        request: &SetupRequest,
        index: &ReleaseIndex,
        observed: &mut SetupObserved,
    ) {
        // This first pass establishes whether a component will be kept and
        // whether setup must bootstrap the vault before a key can be observed.
        let eligible = super::super::planner::plan_setup(observed, request);
        let vault_observable = observed.component_state(Component::Claustrum)
            == ComponentState::Correct
            && !eligible
                .operations
                .iter()
                .any(|step| matches!(step, SetupOperation::BootstrapClaustrum { .. }));
        for &component in &eligible.validation {
            match self.module_steps(component, index, observed, &eligible, vault_observable) {
                Ok((steps, notes)) => {
                    if !steps.is_empty() {
                        observed.module_steps.insert(component, steps);
                    }
                    if !notes.is_empty() {
                        observed.module_notes.insert(component, notes);
                    }
                }
                Err(reason) => {
                    observed
                        .configuration_refusals
                        .entry(component)
                        .or_insert(reason);
                }
            }
        }
    }

    fn module_steps(
        &mut self,
        component: Component,
        index: &ReleaseIndex,
        observed: &SetupObserved,
        eligible: &SetupPlan,
        vault_observable: bool,
    ) -> Result<(Vec<SetupOperation>, Vec<String>), String> {
        let mut steps = Vec::new();
        let mut notes = Vec::new();
        let path = self
            .paths
            .config_path
            .with_file_name(format!("{component}.jsonc"));
        if component == Component::Broca {
            let program = self.auth_program();
            let mut auth = vault::Auth {
                program: &program,
                runner: &mut *self.runner,
                wait: std::thread::sleep,
            };
            let observation = broca::observe(
                &path,
                &self.inventory,
                vault_observable.then_some(&mut auth),
            )?;
            steps.push(SetupOperation::EnsureBrocaKey {
                path: path.clone(),
                observation,
            });
        }
        if !vault::component_grants(component).is_empty() {
            steps.push(SetupOperation::GrantComponentVault { component });
        }
        if component == Component::Cerebellum
            && self.platform == RuntimePlatform::Macos
            && !observed
                .partial_installs
                .get(&component)
                .is_some_and(|partial| !partial.module_matches_release)
        {
            steps.push(SetupOperation::RegisterCerebellumNativeHost);
        }
        let settings = match component {
            Component::Fusiform => settings::plan(&path, &self.inventory, || {
                settings::fusiform_precondition(&self.paths.config_path, &path)
            })?,
            Component::Cerebellum => settings::plan(&path, &self.inventory, || {
                Ok(settings::cerebellum_precondition(self.platform))
            })?,
            _ => settings::Plan::Skip,
        };
        match settings {
            settings::Plan::Write => {
                steps.push(SetupOperation::WriteModuleSettings { component, path })
            }
            settings::Plan::Note(note) => notes.push(note),
            settings::Plan::Skip => {}
        }
        if component == Component::Cerebellum && self.platform == RuntimePlatform::Macos {
            let paths = extension::Paths::new(&self.paths.data_dir);
            if let Some(step) =
                self.extension_step(&paths, eligible, index, observed, &mut notes)?
            {
                steps.push(step);
            }
        }
        Ok((steps, notes))
    }

    fn extension_step(
        &self,
        paths: &extension::Paths,
        eligible: &SetupPlan,
        index: &ReleaseIndex,
        observed: &SetupObserved,
        notes: &mut Vec<String>,
    ) -> Result<Option<SetupOperation>, String> {
        extension::check_ownership(paths, &self.inventory)?;
        let component = Component::Cerebellum;
        let Some(assets) = index
            .components
            .get(component.label())
            .and_then(|entry| entry.assets.get("darwin-arm64"))
        else {
            return Ok(None);
        };
        let (Some(program_asset), Some(zip)) =
            (assets.get("ck-cerebellum"), assets.get(extension::ASSET))
        else {
            return Ok(None);
        };
        let program = self.paths.binary_home.join("ck-cerebellum");
        let installed_digest = program
            .is_file()
            .then(|| {
                components::OWNED_BINARY_KINDS
                    .iter()
                    .find_map(|kind| self.inventory.entry_for_path(kind, &program))
                    .and_then(|row| row.get("archive_sha256").and_then(Value::as_str))
            })
            .flatten();
        let places_program = !observed.partial_installs.contains_key(&component)
            && eligible
                .operations
                .contains(&SetupOperation::InstallComponent { component });
        if !places_program && installed_digest != Some(program_asset.sha256.as_str()) {
            notes.push(EXTENSION_UPGRADE.into());
            return Ok(None);
        }
        Ok(
            extension::plan(paths, &self.inventory, &zip.sha256)?.then(|| {
                SetupOperation::EnsureCerebellumExtension {
                    path: paths.live.clone(),
                    url: zip.url.clone(),
                    sha256: zip.sha256.clone(),
                }
            }),
        )
    }

    pub(super) fn apply_module_step(&mut self, operation: &SetupOperation) -> Result<(), String> {
        let detail = match operation {
            SetupOperation::WriteModuleSettings {
                component: Component::Fusiform,
                path,
            } => settings::ensure_fusiform(&self.paths.config_path, path, &mut self.inventory)?,
            SetupOperation::WriteModuleSettings {
                component: Component::Cerebellum,
                path,
            } => settings::ensure_cerebellum(self.platform, path, &mut self.inventory)?,
            SetupOperation::WriteModuleSettings { component, .. } => {
                return Err(format!("no module settings writer for {component}"))
            }
            SetupOperation::EnsureBrocaKey { path, .. } => {
                let program = self.auth_program();
                let mut auth = vault::Auth {
                    program: &program,
                    runner: &mut *self.runner,
                    wait: std::thread::sleep,
                };
                format!(
                    "Broca deployment key: {}",
                    broca::ensure(path, &mut self.inventory, &mut auth)?
                )
            }
            SetupOperation::GrantComponentVault { component } => {
                let program = self.auth_program();
                let mut auth = vault::Auth {
                    program: &program,
                    runner: &mut *self.runner,
                    wait: std::thread::sleep,
                };
                format!(
                    "{component} vault grants: {}",
                    vault::report(auth.ensure_component(*component)?)
                )
            }
            SetupOperation::RegisterCerebellumNativeHost => {
                if self.platform != RuntimePlatform::Macos {
                    return Ok(());
                }
                let host = self.paths.binary_home.join(NATIVE_HOST);
                let result = self
                    .runner
                    .run(&host.to_string_lossy(), &["--register".into()])?;
                if result.exit_code != Some(0) {
                    return Err(format!(
                        "{NATIVE_HOST} --register failed{}: {}",
                        vault::exit_description(&result),
                        result.explanation()
                    ));
                }
                "Chrome native messaging: ensured".into()
            }
            SetupOperation::EnsureCerebellumExtension { path, url, sha256 } => {
                let paths = extension::Paths::new(&self.paths.data_dir);
                if self.platform != RuntimePlatform::Macos || &paths.live != path {
                    return Err("refusal: Chrome extension step does not match this host".into());
                }
                let changed = extension::ensure(&paths, &mut self.inventory, url, sha256)?;
                format!("Chrome extension at {}: {}. Load this folder in chrome://extensions with Developer mode on; macOS permissions: Accessibility, Screen Recording and Input Monitoring.", path.display(), vault::report(changed))
            }
            _ => unreachable!("non-ensure step passed to module executor"),
        };
        println!("  {detail}");
        Ok(())
    }
}

pub(super) fn refusal_remedy(plan: &SetupPlan, failed: Component, reason: &str) -> String {
    let survivors = plan
        .validation
        .iter()
        .copied()
        .filter(|&component| {
            component != Component::Core && component != failed && !needs(component, failed)
        })
        .collect::<Vec<_>>();
    let roots = survivors
        .iter()
        .copied()
        .filter(|&component| {
            !survivors
                .iter()
                .copied()
                .any(|other| other != component && needs(other, component))
        })
        .map(Component::label)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        format!("{failed}: {reason}. No remaining optional components to retry; rerun the same ck setup command after fixing the reason above.")
    } else {
        format!(
            "{failed}: {reason}\nRetry the remaining components: ck setup --with {}",
            roots.join(",")
        )
    }
}

fn needs(component: Component, target: Component) -> bool {
    components::component_needs(component)
        .iter()
        .any(|&need| need == target || needs(need, target))
}
