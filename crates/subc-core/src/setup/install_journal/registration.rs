use super::{files, Action, Journal, Registration, Step, Undo};
use crate::setup::runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform};
use crate::setup::systemd_unit;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

pub(in crate::setup) struct Service<'a, R> {
    pub platform: RuntimePlatform,
    pub paths: &'a RuntimePaths,
    pub runner: &'a mut R,
}

impl<R: CommandRunner> Service<'_, R> {
    pub fn prepare(
        &mut self,
        journal: &mut Journal,
        before: Registration,
        after: Registration,
    ) -> Result<usize, String> {
        if !self.matches(&before)? {
            return Err(self.refuse(
                &before.definition,
                "service manager does not report the recorded pre-install state",
            ));
        }
        journal.prepare(Step {
            target: self.paths.definition.clone(),
            action: Action::Registration { before, after },
        })
    }

    pub fn record(
        &self,
        definition: &Path,
        registered: bool,
        sha256: Option<String>,
    ) -> Registration {
        Registration {
            registered,
            definition: definition.to_owned(),
            definition_sha256: sha256,
            program: self.paths.daemon.clone(),
            arguments: vec![
                "--config-store-root".into(),
                self.paths.config_store_root.to_string_lossy().into_owned(),
            ],
        }
    }

    pub fn install(
        &mut self,
        inventory: &mut crate::setup::inventory::Inventory,
    ) -> Result<(), String> {
        use sha2::{Digest, Sha256};
        let state = runtime::observe(self.platform, self.runner)?;
        if let crate::setup::model::RuntimeState::Unowned { reason } =
            runtime::definition_state(self.platform, self.paths, state, self.runner, inventory)?
        {
            return Err(format!("refusal: {reason}"));
        }
        let root = std::path::absolute(&self.paths.config_store_root)
            .map_err(|error| format!("could not resolve config store path: {error}"))?;
        let store_parent = root.parent().ok_or("config store root has no parent")?;
        #[cfg(unix)]
        crate::setup::file_modes::ensure_owned_directory(store_parent, 0o700)?;
        #[cfg(windows)]
        subc_os::windows_acl::create_private_dir(store_parent)
            .map_err(|error| self.refuse(store_parent, &error.to_string()))?;
        let desired = runtime::desired_definition_checked(self.platform, self.paths)?;
        #[cfg(unix)]
        if self.platform == RuntimePlatform::Linux {
            if let Some(old) = systemd_unit::recorded_elsewhere(&self.paths.definition, inventory) {
                return move_unit(self, inventory, &old, &desired);
            }
        }
        let current = files::digest(&self.paths.definition)?;
        let mut before = self.record(&self.paths.definition, state.registered, current.clone());
        if self.platform == RuntimePlatform::Macos && state.registered {
            let old = std::fs::read_to_string(&self.paths.definition)
                .map_err(|error| self.refuse(&self.paths.definition, &error.to_string()))?;
            let canonical = crate::setup::definition::canonical(self.platform, &old)?;
            let plist: serde_json::Value =
                serde_json::from_str(&canonical).map_err(|error| error.to_string())?;
            let entries = plist["ProgramArguments"]
                .as_array()
                .ok_or("LaunchAgent has no ProgramArguments")?;
            let mut arguments = entries.iter().map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or("LaunchAgent contains a non-string program argument")
            });
            before.program = arguments
                .next()
                .ok_or("LaunchAgent has no program")??
                .into();
            before.arguments = arguments.collect::<Result<Vec<_>, _>>()?;
        }
        let digest = format!("{:x}", Sha256::digest(desired.as_bytes()));
        let after = self.record(&self.paths.definition, true, Some(digest.clone()));
        let transaction = inventory
            .transaction()
            .ok_or("runtime change requires an install transaction")?;
        let index = self.prepare(transaction.journal(), before, after)?;
        if current.as_ref() != Some(&digest) {
            transaction.write(&self.paths.definition, desired.as_bytes(), 0o644)?;
        }
        self.replay(transaction.journal().step(index))?;
        transaction.journal().done(index)?;
        for (kind, field, value) in [
            (
                "runtime-definition",
                "definition_semantics",
                crate::setup::definition::canonical(self.platform, &desired)?,
            ),
            (
                "runtime-registration",
                "identifier",
                self.platform.identifier().to_owned(),
            ),
        ] {
            inventory.remove_owned_path(kind, &self.paths.definition);
            inventory.record(
                kind,
                &self.paths.definition,
                serde_json::Map::from_iter([(field.into(), value.into())]),
            );
        }
        Ok(())
    }

    pub fn matches(&mut self, expected: &Registration) -> Result<bool, String> {
        self.matches_loaded(expected, true)
    }

    fn matches_loaded(
        &mut self,
        expected: &Registration,
        require_reload: bool,
    ) -> Result<bool, String> {
        if runtime::observe(self.platform, self.runner)?.registered != expected.registered {
            return Ok(false);
        }
        if !expected.registered {
            return Ok(true);
        }
        match self.platform {
            RuntimePlatform::Linux => {
                let args = [
                    "--user",
                    "show",
                    "-p",
                    "FragmentPath",
                    "-p",
                    "NeedDaemonReload",
                    self.platform.identifier(),
                ]
                .map(str::to_owned);
                let result = self.runner.run("systemctl", &args)?;
                if !result.success {
                    return Err(self.refuse(&expected.definition, &result.explanation()));
                }
                let fragment =
                    property(&result.stdout, "FragmentPath").filter(|path| !path.is_empty());
                Ok(
                    fragment.is_some_and(|path| same_file(Path::new(path), &expected.definition))
                        && (!require_reload
                            || property(&result.stdout, "NeedDaemonReload") == Some("no")),
                )
            }
            RuntimePlatform::Macos => {
                let result = self.runner.run(
                    "launchctl",
                    &[
                        "print".into(),
                        format!(
                            "gui/{}/{}",
                            runtime::current_uid(),
                            self.platform.identifier()
                        ),
                    ],
                )?;
                if !result.success {
                    return Err(self.refuse(&expected.definition, &result.explanation()));
                }
                Ok(launchd_arguments(
                    &result.stdout,
                    &expected.program,
                    &expected.arguments,
                ))
            }
            // Task Scheduler may retain account and settings an update must
            // preserve. A registered status alone cannot prove a same-name
            // replacement. Require a durable journal completion record from
            // the command that successfully updated the task.
            RuntimePlatform::Windows => Ok(true),
        }
    }

    pub fn verify(&mut self, step: &Step, done: bool) -> Result<bool, String> {
        let (before, after) = registration(step);
        if self.needs_done_mark(before, after) && !done {
            return Ok(false);
        }
        self.matches(after)
    }

    pub fn can_replay(&mut self, step: &Step) -> Result<bool, String> {
        let (before, after) = registration(step);
        // Do not replay if the manager cannot distinguish the original
        // registration from the update; undo from the recorded definition.
        if self.needs_done_mark(before, after) {
            return Ok(false);
        }
        self.matches(before)
    }

    pub fn replay(&mut self, step: &Step) -> Result<(), String> {
        let (before, after) = registration(step);
        self.apply(before, after)
    }

    pub fn apply(&mut self, before: &Registration, after: &Registration) -> Result<(), String> {
        let paths = self.at(&after.definition);
        if after.registered {
            self.check_definition(after)?;
            if self.platform == RuntimePlatform::Linux {
                self.enable_unit(&after.definition)?;
            } else if before.registered && self.platform == RuntimePlatform::Windows {
                runtime::update_windows_task(&paths, self.runner)?;
            } else {
                if before.registered && self.platform == RuntimePlatform::Macos {
                    runtime::deregister(
                        self.platform,
                        &self.at(&before.definition),
                        None,
                        self.runner,
                    )?;
                }
                runtime::register(self.platform, &paths, self.runner)?;
            }
        } else if self.platform == RuntimePlatform::Linux {
            systemd_unit::verify_fragment(&before.definition, self.runner)?;
            self.systemctl(&["--user", "disable", self.platform.identifier()])?;
        } else {
            runtime::deregister(
                self.platform,
                &self.at(&before.definition),
                None,
                self.runner,
            )?;
        }
        if !self.matches(after)? {
            return Err(self.refuse(
                &after.definition,
                "service manager did not load the recorded definition",
            ));
        }
        Ok(())
    }

    pub fn undo(&mut self, step: &Step, phase: Undo) -> Result<(), String> {
        let (before, after) = registration(step);
        match phase {
            Undo::Quiesce => {
                if self.platform == RuntimePlatform::Windows
                    && before.registered
                    && after.registered
                {
                    // Keep the task: after the definition is restored, patch
                    // back just the generated fields without losing settings
                    // the user's scheduler preserved across the upgrade.
                    return Ok(());
                }
                if self.matches_loaded(before, false)? && !self.needs_done_mark(before, after) {
                    return Ok(());
                }
                let observed = runtime::observe(self.platform, self.runner)?;
                if !observed.registered {
                    return Ok(());
                }
                if !self.matches(after)? {
                    return Err(self.refuse(
                        &after.definition,
                        "manager loaded a different registration; recovery stopped",
                    ));
                }
                runtime::deregister(
                    self.platform,
                    &self.at(&after.definition),
                    None,
                    self.runner,
                )?;
            }
            Undo::Restore => {}
            Undo::Reactivate => {
                if before.registered {
                    self.check_definition(before)?;
                }
                if self.platform == RuntimePlatform::Windows
                    && before.registered
                    && after.registered
                {
                    return runtime::update_windows_task(&self.at(&before.definition), self.runner);
                }
                if self.platform == RuntimePlatform::Linux {
                    self.reload()?;
                    if before.registered && !self.matches(before)? {
                        self.enable_unit(&before.definition)?;
                    }
                }
                if !self.matches(before)? {
                    if !before.registered {
                        return Err(self
                            .refuse(&before.definition, "unexpected registration after rollback"));
                    }
                    runtime::register(self.platform, &self.at(&before.definition), self.runner)?;
                    if !self.matches(before)? {
                        return Err(self.refuse(
                            &before.definition,
                            "previous registration could not be verified",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn needs_done_mark(&self, before: &Registration, after: &Registration) -> bool {
        if !before.registered || !after.registered || before == after {
            return false;
        }
        match self.platform {
            RuntimePlatform::Linux => false,
            RuntimePlatform::Windows => true,
            // launchctl's program and arguments distinguish a replacement
            // only when it loads a different invocation, not a policy change.
            RuntimePlatform::Macos => {
                before.program == after.program && before.arguments == after.arguments
            }
        }
    }

    fn enable_unit(&mut self, definition: &Path) -> Result<(), String> {
        self.reload()?;
        systemd_unit::verify_fragment(definition, self.runner)?;
        self.systemctl(&["--user", "enable", self.platform.identifier()])
    }

    fn reload(&mut self) -> Result<(), String> {
        self.systemctl(&["--user", "daemon-reload"])
    }

    fn systemctl(&mut self, args: &[&str]) -> Result<(), String> {
        let result = self.runner.run(
            "systemctl",
            &args.iter().copied().map(str::to_owned).collect::<Vec<_>>(),
        )?;
        if !result.success {
            return Err(self.refuse(&self.paths.definition, &result.explanation()));
        }
        Ok(())
    }

    fn check_definition(&self, state: &Registration) -> Result<(), String> {
        if files::digest(&state.definition)? != state.definition_sha256
            || state.definition_sha256.is_none()
        {
            return Err(self.refuse(
                &state.definition,
                "definition does not have the bytes recorded in the journal",
            ));
        }
        Ok(())
    }

    fn at(&self, definition: &Path) -> RuntimePaths {
        RuntimePaths {
            definition: definition.to_owned(),
            ..self.paths.clone()
        }
    }

    fn refuse(&self, path: &Path, detail: &str) -> String {
        format!(
            "refusal: {} registration at {}: {detail}",
            self.platform.identifier(),
            path.display()
        )
    }
}

pub(in crate::setup) fn refuse_stopped_launch_agent(
    platform: RuntimePlatform,
    status: runtime::RuntimeStatus,
) -> Result<(), String> {
    if platform == RuntimePlatform::Macos && status.registered && !status.live {
        let job = format!("gui/{}/{}", runtime::current_uid(), platform.identifier());
        return Err(format!("refusal: LaunchAgent {} is registered but not running; start it with `launchctl kickstart \"gui/$(id -u)/{}\"` or remove it with `launchctl bootout {job}`, then run ck setup again", platform.identifier(), platform.identifier()));
    }
    Ok(())
}

fn registration(step: &Step) -> (&Registration, &Registration) {
    let Action::Registration { before, after } = &step.action else {
        unreachable!("registration requires a manager step")
    };
    (before, after)
}

fn property<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn same_file(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn launchd_arguments(text: &str, program: &Path, expected: &[String]) -> bool {
    let executable = program.to_string_lossy();
    let lines = || text.lines().map(str::trim);
    if !lines().any(|line| line.strip_prefix("program = ") == Some(&executable)) {
        return false;
    }
    let arguments: Vec<&str> = lines()
        .skip_while(|line| *line != "arguments = {")
        .skip(1)
        .take_while(|line| *line != "}")
        .collect();
    arguments.first() == Some(&executable.as_ref())
        && arguments[1..]
            .iter()
            .copied()
            .eq(expected.iter().map(String::as_str))
}

#[cfg(unix)]
pub(in crate::setup) fn move_unit(
    service: &mut Service<'_, impl CommandRunner>,
    inventory: &mut crate::setup::inventory::Inventory,
    old: &Path,
    desired: &str,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let new = &service.paths.definition;
    systemd_unit::refuse_foreign_fragment(new, &[old.to_owned()], service.runner)?;
    let row = inventory
        .entry_for_path("runtime-definition", old)
        .ok_or_else(|| service.refuse(old, "old unit is not owned by this installation"))?;
    let old_text =
        std::fs::read_to_string(old).map_err(|error| service.refuse(old, &error.to_string()))?;
    if !runtime::definition_trusted(service.platform, &service.at(old), row, &old_text) {
        return Err(service.refuse(old, "old unit no longer matches the installed definition"));
    }
    let old_digest =
        files::digest(old)?.ok_or_else(|| service.refuse(old, "recorded old unit disappeared"))?;
    let registered = runtime::observe(service.platform, service.runner)?.registered;
    let before = service.record(old, registered, Some(old_digest));
    let after = service.record(
        new,
        true,
        Some(format!("{:x}", Sha256::digest(desired.as_bytes()))),
    );
    if files::digest(new)?.is_some() {
        return Err(service.refuse(new, "refusing to overwrite a second unit definition"));
    }
    let journal = inventory
        .transaction()
        .ok_or("unit move requires an install transaction")?
        .journal();
    files::place(
        journal,
        new,
        files::Kind::File,
        files::Content::Bytes(desired.as_bytes(), 0o644),
    )?;
    let index = service.prepare(journal, before, after)?;
    service.replay(journal.step(index))?;
    journal.done(index)?;
    for link in old_unit_links(old)? {
        super::symlink::place(journal, &link, None)?;
    }
    files::place(journal, old, files::Kind::File, files::Content::Absent)?;
    service.reload()?;
    for kind in ["runtime-definition", "runtime-registration"] {
        if let Some(entry) = inventory.entry_for_path(kind, old) {
            let mut fields = entry
                .as_object()
                .expect("inventory row is an object")
                .clone();
            fields.remove("kind");
            fields.remove("path");
            fields.insert(
                "moved_from".into(),
                old.to_string_lossy().into_owned().into(),
            );
            if kind == "runtime-definition" {
                fields.insert(
                    "definition_semantics".into(),
                    crate::setup::definition::canonical(service.platform, desired)?.into(),
                );
            }
            inventory.remove_owned_path(kind, old);
            inventory.record(kind, new, fields);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn old_unit_links(unit: &Path) -> Result<Vec<PathBuf>, String> {
    use std::{fs, io};
    let directory = unit.parent().ok_or("unit has no containing directory")?;
    let basename = unit.file_name().ok_or("unit has no file name")?;
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".wants") || name.ends_with(".requires"))
            || !entry.file_type().is_ok_and(|info| info.is_dir())
        {
            continue;
        }
        let link = entry.path().join(basename);
        let target = match fs::read_link(&link) {
            Ok(target) if target.is_absolute() => target,
            Ok(target) => entry.path().join(target),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                continue
            }
            Err(error) => return Err(format!("cannot read {}: {error}", link.display())),
        };
        if same_file(&target, unit) {
            paths.push(link);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests;
