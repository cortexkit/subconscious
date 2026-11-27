use super::*;
use crate::setup::install_journal::{activation, files, system::SystemRecovery, Build, Live};

#[derive(Debug)]
pub(super) struct RollbackPrompt {
    snapshot: crate::setup::prompt::PromptSnapshot,
    prior: Value,
    runtime: Result<super::super::runtime::RuntimeStatus, String>,
    paths: super::super::runtime::RuntimePaths,
    live: BTreeMap<String, Result<Option<Live>, String>>,
}

impl SystemUpgradeBackend {
    pub(super) fn pause_app_rollback(&mut self) -> Result<(), String> {
        if self.app_hold.is_none() {
            return Ok(());
        }
        let paths = self.runtime_paths()?;
        self.pause_app_rollback_at(paths)
    }

    fn pause_app_rollback_at(
        &mut self,
        paths: super::super::runtime::RuntimePaths,
    ) -> Result<(), String> {
        let platform = super::super::runtime::RuntimePlatform::current();
        let snapshot = crate::setup::prompt::PromptSnapshot::capture(
            self.inventory.manifest(),
            self.targets
                .values()
                .map(|entry| entry.destination.clone())
                .chain([self.config_path.clone(), paths.definition.clone()]),
        )?;
        let runtime = super::super::runtime::observe(platform, &mut self.runner);
        let live = self.rollback_activation_observations(&paths);
        let prior = self
            .inventory
            .suspend_transaction()
            .ok_or("refusal: no journaled upgrade is available to roll back")?;
        self.rollback_prompt = Some(RollbackPrompt {
            snapshot,
            prior,
            runtime,
            paths,
            live,
        });
        self.inventory.release_lock();
        self.app_hold.take();
        Ok(())
    }

    pub(super) fn resume_app_rollback(&mut self, decision: RollbackDecision) -> Result<(), String> {
        let Some(prompt) = self.rollback_prompt.take() else {
            return Ok(());
        };
        let hold = super::super::install_lock::InstallHold::take(
            self.inventory.manifest(),
            "ck upgrade (app)",
            super::super::install_lock::WAIT,
        )?;
        prompt.snapshot.check()?;
        if decision == RollbackDecision::Accepted {
            self.check_rollback_liveness(&prompt)?;
        }
        self.inventory.resume_transaction(prompt.prior)?;
        self.app_hold = Some(hold);
        Ok(())
    }

    fn check_rollback_liveness(&mut self, prompt: &RollbackPrompt) -> Result<(), String> {
        let platform = super::super::runtime::RuntimePlatform::current();
        let runtime = super::super::runtime::observe(platform, &mut self.runner)?;
        let before = prompt.runtime.as_ref().map_err(|error| format!("refusal: could not read daemon service state before rollback confirmation: {error}; rerun ck upgrade"))?;
        if runtime.live != before.live || runtime.registered != before.registered {
            return Err("refusal: the daemon service state changed while waiting for rollback confirmation; rerun ck upgrade".into());
        }
        let current = self.rollback_activation_observations(&prompt.paths);
        let changed: Vec<_> = prompt
            .live
            .iter()
            .filter_map(|(name, old)| (current.get(name) != Some(old)).then_some(name.as_str()))
            .collect();
        if !changed.is_empty() {
            return Err(format!("refusal: the running identity or build of {} changed while waiting for confirmation; rerun ck upgrade", changed.join(", ")));
        }
        Ok(())
    }

    fn rollback_activation_observations(
        &mut self,
        paths: &super::super::runtime::RuntimePaths,
    ) -> BTreeMap<String, Result<Option<Live>, String>> {
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: self.subc.as_deref(),
            platform: super::super::runtime::RuntimePlatform::current(),
            paths,
            runner: &mut self.runner,
        };
        self.targets
            .values()
            .filter(|item| self.journal_activations.contains_key(item.target.label()))
            .map(|item| {
                let mut control = recovery.control(&item.destination, None);
                (
                    item.target.label().into(),
                    activation::Control::live(&mut control, item.target.module_id()),
                )
            })
            .collect()
    }
}

impl SystemUpgradeBackend {
    pub(super) fn runtime_paths(&self) -> Result<super::super::runtime::RuntimePaths, String> {
        let root = self
            .inventory
            .manifest()
            .parent()
            .ok_or("installer manifest has no parent")?;
        let platform = super::super::runtime::RuntimePlatform::current();
        Ok(super::super::runtime::session_runtime_paths(
            platform,
            &root.join("bin"),
            &super::super::apply::user_home()?,
        ))
    }

    pub(super) fn prepare_journal_target(
        &mut self,
        target: UpgradeTarget,
        candidate: &Path,
    ) -> Result<(), String> {
        let destination = self.target(target)?.destination.clone();
        let module = target
            .module_id()
            .filter(|_| self.is_module_supervised(target));
        if !target.is_daemon() && module.is_none() {
            return Ok(());
        }
        let paths = self.runtime_paths()?;
        let platform = super::super::runtime::RuntimePlatform::current();
        if target.is_daemon() && platform == super::super::runtime::RuntimePlatform::Macos {
            let status = super::super::runtime::observe(platform, &mut self.runner)?;
            super::super::install_journal::registration::refuse_stopped_launch_agent(
                platform, status,
            )?;
        }
        let build = if target.is_daemon() {
            activation::installed_daemon_build(candidate)?
        } else {
            Build::Image {
                sha256: files::digest(candidate)?.ok_or("upgrade candidate is missing")?,
            }
        };
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: self.subc.as_deref(),
            platform,
            paths: &paths,
            runner: &mut self.runner,
        };
        let previous = activation::capture(&mut recovery.control(&destination, None), module)?;
        let transaction = self
            .inventory
            .transaction()
            .ok_or("upgrade target has no install transaction")?;
        let index = activation::prepare_with_before(
            transaction.journal(),
            &destination,
            module,
            previous,
            Some(build),
        )?;
        self.journal_activations
            .insert(target.label().to_owned(), index);
        Ok(())
    }

    pub(super) fn verify_journal_activation(
        &mut self,
        target: UpgradeTarget,
    ) -> Result<(), String> {
        let Some(&index) = self.journal_activations.get(target.label()) else {
            return Ok(());
        };
        let path = self.target(target)?.destination.clone();
        let paths = self.runtime_paths()?;
        let platform = super::super::runtime::RuntimePlatform::current();
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: self.subc.as_deref(),
            platform,
            paths: &paths,
            runner: &mut self.runner,
        };
        let transaction = self
            .inventory
            .transaction()
            .ok_or("upgrade target has no install transaction")?;
        let step = transaction.journal().step(index);
        let mut control = recovery.control(&path, None);
        activation::wait_verified(step, &mut control)?;
        let reported_build = target
            .module_id()
            .and_then(|module| control.reported_module_build(module));
        transaction.journal().done_with_build(index, reported_build)
    }

    pub(super) fn journal_place(
        &mut self,
        target: UpgradeTarget,
        destination: &Path,
        prepared: &PreparedUpgradeAsset,
    ) -> Result<String, String> {
        self.prepare_journal_target(target, &prepared.candidate)?;
        let new_destination = if target.is_daemon() && self.platform == AlphaTarget::DarwinArm64 {
            let home = super::super::daemon_bundle::binary_home(destination)?;
            super::super::daemon_bundle::executable(&home)
        } else {
            destination.to_owned()
        };
        if new_destination != destination {
            super::super::daemon_bundle::check_destination(&new_destination, &self.inventory)?;
        }
        let transaction = self
            .inventory
            .transaction()
            .ok_or("upgrade target has no install transaction")?;
        #[cfg(unix)]
        if target.is_daemon() && self.platform == AlphaTarget::DarwinArm64 {
            let source = super::super::daemon_bundle::containing_bundle(&prepared.candidate)
                .ok_or("archive has no daemon bundle")?;
            let target_bundle = super::super::daemon_bundle::containing_bundle(&new_destination)
                .ok_or("destination is not in its managed app bundle")?;
            super::super::install_journal::bundle::place(
                transaction.journal(),
                target_bundle,
                source,
                &mut self.runner,
            )?;
        } else {
            transaction.replace(destination, &prepared.candidate)?;
        }
        #[cfg(not(unix))]
        transaction.replace(destination, &prepared.candidate)?;
        if new_destination != destination {
            for kind in super::super::components::OWNED_BINARY_KINDS {
                if let Some(fields) = self
                    .inventory
                    .entry_for_path(kind, destination)
                    .and_then(Value::as_object)
                    .cloned()
                {
                    self.inventory.record(kind, &new_destination, fields);
                }
            }
            self.targets
                .get_mut(target.label())
                .expect("upgrade target")
                .destination = new_destination.clone();
        }
        let destination = new_destination.as_path();
        self.activated_inodes
            .insert(target.label().into(), destination_inode(destination)?);
        self.record_replacement_digest(target, destination, Some(&prepared.archive_sha256))?;
        if target.is_self_replacing() {
            super::super::ckni::refresh_after_ck_replacement(
                destination,
                &mut self.inventory,
                super::super::ckni::Mechanism::CURRENT,
            )?;
        }
        if target.is_daemon() {
            if let Some(nats) = self.core_cut.nats.as_mut() {
                nats.apply_recorded(&mut self.inventory)?;
            }
        }
        Ok(format!(
            "destination replaced; inode={}",
            destination_inode(destination)?
        ))
    }

    pub(super) fn close_upgrade_journal(&mut self, commit: bool) -> Result<(), String> {
        let paths = self.runtime_paths()?;
        let mut recovery = SystemRecovery {
            executable: &self.executable,
            connection: self.subc.as_deref(),
            platform: super::super::runtime::RuntimePlatform::current(),
            paths: &paths,
            runner: &mut self.runner,
        };
        if commit {
            self.inventory.commit_transaction(&mut recovery)?;
        } else {
            self.inventory.undo_transaction(&mut recovery)?;
        }
        self.journal_activations.clear();
        self.activated_inodes.clear();
        Ok(())
    }

    pub(super) fn retire_flat_daemon(&mut self) -> Result<(), String> {
        if self.platform != AlphaTarget::DarwinArm64 {
            return Ok(());
        }
        let root = self
            .inventory
            .manifest()
            .parent()
            .ok_or("installer manifest has no parent")?;
        let path = root.join("bin/ck-subc");
        for kind in super::super::components::OWNED_BINARY_KINDS {
            if let Some(row) = self.inventory.entry_for_path(kind, &path) {
                let expected = row.get("sha256").and_then(Value::as_str);
                if let Some(digest) = files::digest(&path)? {
                    if expected != Some(&digest) {
                        return Err(format!(
                            "refusal: legacy daemon {} differs from the inventory",
                            path.display()
                        ));
                    }
                    self.inventory
                        .transaction()
                        .ok_or("legacy retirement has no install transaction")?
                        .remove(&path)?;
                }
                self.inventory.remove_owned_path(kind, &path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::setup::runtime::{CommandResult, CommandRunner, RuntimePlatform};

    struct Stopped;
    impl CommandRunner for Stopped {
        fn run(&mut self, _: &str, _: &[String]) -> Result<CommandResult, String> {
            Ok(CommandResult::default())
        }
    }

    #[test]
    fn keeping_an_unconfirmed_release_ignores_a_liveness_change_at_the_prompt() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };

        struct Liveness(Arc<AtomicBool>);
        impl CommandRunner for Liveness {
            fn run(&mut self, _: &str, args: &[String]) -> Result<CommandResult, String> {
                let live = self.0.load(Ordering::Relaxed);
                Ok(CommandResult {
                    success: live || !args.iter().any(|arg| arg == "is-active"),
                    exit_code: Some(if live || !args.iter().any(|arg| arg == "is-active") {
                        0
                    } else {
                        1
                    }),
                    stdout: if live {
                        "state = running\nStatus: Running".into()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                })
            }
        }
        let root = cortexkit_test_support::ScratchDir::new("app-rollback-keep");
        let target = super::super::tests::upgrade_target("ck-aft");
        let path = root.join("ck-aft");
        fs::write(&path, b"candidate").unwrap();
        let mut backend = super::super::tests::managed_backend(&root, target, &path);
        let live = Arc::new(AtomicBool::new(false));
        backend.runner = Box::new(Liveness(live.clone()));
        let manifest = backend.inventory.manifest().to_owned();
        backend
            .retain_app_hold(InstallHold::take(&manifest, "app fixture", Duration::ZERO).unwrap());
        backend.inventory.start_transaction().unwrap();
        backend
            .inventory
            .transaction()
            .unwrap()
            .write(&path, b"unconfirmed", 0o600)
            .unwrap();
        let paths = super::super::super::runtime::runtime_paths(
            RuntimePlatform::current(),
            &root.join("bin"),
            root.path(),
        );
        backend.pause_app_rollback_at(paths).unwrap();
        live.store(true, Ordering::Relaxed);
        backend
            .resume_app_rollback(RollbackDecision::Declined)
            .unwrap();
        backend.inventory.leave_transaction_unconfirmed().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"unconfirmed");
        assert!(super::super::super::install_journal::Journal::exists(&manifest).unwrap());
    }

    #[test]
    fn an_upgrade_rollback_answer_cannot_overwrite_another_installers_recovery() {
        let root = cortexkit_test_support::ScratchDir::new("app-rollback-confirmation");
        let target = super::super::tests::upgrade_target("ck-aft");
        let path = root.join("ck-aft");
        fs::write(&path, b"candidate").unwrap();
        let mut backend = super::super::tests::managed_backend(&root, target, &path);
        backend.runner = Box::new(Stopped);
        let manifest = backend.inventory.manifest().to_owned();
        let hold = InstallHold::take(&manifest, "app fixture", Duration::ZERO).unwrap();
        backend.retain_app_hold(hold);
        backend.inventory.start_transaction().unwrap();
        backend
            .inventory
            .transaction()
            .unwrap()
            .write(&path, b"unconfirmed", 0o600)
            .unwrap();
        let paths = super::super::super::runtime::runtime_paths(
            RuntimePlatform::current(),
            &root.join("bin"),
            root.path(),
        );
        backend.pause_app_rollback_at(paths).unwrap();
        assert!(!InstallHold::is_held(&manifest));
        let other = InstallHold::take(&manifest, "other fixture", Duration::ZERO).unwrap();
        super::super::super::install_journal::bootstrap::recover_locked(
            &manifest,
            &mut super::super::super::install_journal::bootstrap::FileRecovery,
            false,
        )
        .unwrap();
        drop(other);
        let refusal = backend
            .resume_app_rollback(RollbackDecision::Accepted)
            .unwrap_err();
        assert!(refusal.contains("installer.journal"), "{refusal}");
        assert!(refusal.contains("changed while waiting for confirmation"));
        assert_eq!(fs::read(path).unwrap(), b"candidate");
    }
}
