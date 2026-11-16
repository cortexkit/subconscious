//! The candidate daemon bundle is staged and verified before the installed bundle moves.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    file_modes,
    inventory::Inventory,
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform},
};

pub(super) const NAME: &str = "CortexKit Daemon.app";
const APP_ID: &str = "5R5846NBPW.io.cortexkit.daemon";
const EXECUTABLE: &str = "Contents/MacOS/ck-subc";

pub(super) fn bundle_path(binary_home: &Path) -> PathBuf {
    binary_home
        .parent()
        .unwrap_or(binary_home)
        .join("apps")
        .join(NAME)
}

pub(super) fn executable(binary_home: &Path) -> PathBuf {
    bundle_path(binary_home).join(EXECUTABLE)
}

pub(super) fn check_destination(executable: &Path, inventory: &Inventory) -> Result<(), String> {
    let Some(bundle) = containing_bundle(executable) else {
        return Ok(());
    };
    match fs::symlink_metadata(bundle) {
        Ok(metadata) => {
            let owned = super::components::OWNED_BINARY_KINDS
                .iter()
                .any(|kind| inventory.owns_path(kind, executable));
            if !owned || !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!("refusal: app bundle {} is not owned by setup; move it aside and run ck setup again", bundle.display()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect app bundle {}: {error}",
            bundle.display()
        )),
    }
}

pub(super) fn containing_bundle(executable: &Path) -> Option<&Path> {
    let bundle = executable.ancestors().nth(3)?;
    (bundle.file_name().is_some_and(|name| name == NAME) && bundle.join(EXECUTABLE) == executable)
        .then_some(bundle)
}

pub(super) fn binary_home(daemon: &Path) -> Result<PathBuf, String> {
    if let Some(bundle) = containing_bundle(daemon) {
        return bundle
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("bin"))
            .ok_or_else(|| "daemon bundle has no data directory".into());
    }
    daemon
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "daemon has no binary directory".into())
}

fn run<R: CommandRunner + ?Sized>(
    runner: &mut R,
    program: &str,
    args: &[String],
) -> Result<String, String> {
    let result = runner.run(program, args)?;
    if !result.success {
        return Err(format!("{program}: {}", result.explanation()));
    }
    Ok(result.stdout)
}

pub(super) fn extract<R: CommandRunner + ?Sized>(
    archive: &Path,
    destination: &Path,
    runner: &mut R,
) -> Result<(), String> {
    run(
        runner,
        "ditto",
        &[
            "-x".into(),
            "-k".into(),
            archive.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ],
    )?;
    Ok(())
}

pub(super) struct Replacement {
    bundle: PathBuf,
    previous: Option<PathBuf>,
}

impl Replacement {
    pub(super) fn place<R: CommandRunner + ?Sized>(
        candidate: &Path,
        binary_home: &Path,
        runner: &mut R,
    ) -> Result<Self, String> {
        let source =
            containing_bundle(candidate).ok_or("daemon archive did not contain the app bundle")?;
        let bundle = bundle_path(binary_home);
        let staged = bundle.with_extension("app.new");
        let previous = bundle.with_extension("app.rollback");
        let plist = staged.with_extension("profile.plist");
        // A leftover here means an earlier replacement stopped part way. Never
        // guess which copy is good: name it and let the operator decide.
        if let Some(occupied) = [&staged, &previous, &plist]
            .into_iter()
            .find(|path| fs::symlink_metadata(path).is_ok())
        {
            return Err(format!(
                "refusal: {} is left over from an interrupted daemon update; check that {} runs, remove the leftover, and run the command again",
                occupied.display(),
                bundle.display()
            ));
        }
        file_modes::ensure_owned_directory(bundle.parent().unwrap(), 0o755)?;
        let stage = (|| {
            run(
                runner,
                "ditto",
                &[
                    source.to_string_lossy().into_owned(),
                    staged.to_string_lossy().into_owned(),
                ],
            )?;
            run(
                runner,
                "codesign",
                &[
                    "--verify".into(),
                    "--strict".into(),
                    staged.to_string_lossy().into_owned(),
                ],
            )
            .map_err(|error| {
                format!("refusal: daemon bundle signature verification failed: {error}")
            })?;
            let profile = staged.join("Contents/embedded.provisionprofile");
            let decoded = run(
                runner,
                "security",
                &[
                    "cms".into(),
                    "-D".into(),
                    "-i".into(),
                    profile.to_string_lossy().into_owned(),
                ],
            )?;
            // Decode outside the signed bundle: an extra file inside it would
            // invalidate the resource seal we just checked.
            file_modes::write_atomic(&plist, decoded.as_bytes(), 0o600, 0o600, "profile.tmp")?;
            let identifier = run(
                runner,
                "/usr/libexec/PlistBuddy",
                &[
                    "-c".into(),
                    "Print :Entitlements:com.apple.application-identifier".into(),
                    plist.to_string_lossy().into_owned(),
                ],
            );
            let _ = fs::remove_file(&plist);
            if identifier?.trim() != APP_ID {
                return Err("refusal: embedded profile application-identifier does not match 5R5846NBPW.io.cortexkit.daemon".into());
            }
            Ok(())
        })();
        if let Err(error) = stage {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
        let previous = if bundle.exists() {
            if let Err(error) = fs::rename(&bundle, &previous) {
                let _ = fs::remove_dir_all(&staged);
                return Err(format!(
                    "could not preserve previous daemon bundle: {error}"
                ));
            }
            Some(previous)
        } else {
            None
        };
        if let Err(error) = fs::rename(&staged, &bundle) {
            if let Some(previous) = &previous {
                fs::rename(previous, &bundle).map_err(|restore| {
                    format!("bundle replacement failed: {error}; restore failed: {restore}")
                })?;
            }
            let _ = fs::remove_dir_all(staged);
            return Err(format!("could not place daemon bundle: {error}"));
        }
        Ok(Self { bundle, previous })
    }

    pub(super) fn rollback(&mut self) -> Result<(), String> {
        if self.bundle.exists() {
            fs::remove_dir_all(&self.bundle)
                .map_err(|error| format!("could not remove replacement daemon bundle: {error}"))?;
        }
        if let Some(previous) = &self.previous {
            fs::rename(previous, &self.bundle)
                .map_err(|error| format!("could not restore daemon bundle: {error}"))?;
            self.previous = None;
        }
        Ok(())
    }

    pub(super) fn complete(&mut self) -> Result<(), String> {
        if let Some(previous) = &self.previous {
            fs::remove_dir_all(previous)
                .map_err(|error| format!("could not remove previous daemon bundle: {error}"))?;
            self.previous = None;
        }
        Ok(())
    }
}

/// The bundle, inventory rows identifying setup-owned executables, and launch
/// agent share one rollback boundary.
/// Legacy executables stay in place until the caller verifies the running daemon.
pub(super) struct Migration {
    replacement: Option<Replacement>,
    paths: RuntimePaths,
    definition: Option<runtime::DefinitionSnapshot>,
    legacy: super::retained_binary::RetainedBinary,
}

impl Migration {
    pub(super) fn old_destination(&self) -> &Path {
        &self.legacy.old
    }
    pub(super) fn capture(
        paths: &RuntimePaths,
        old_destination: &Path,
        inventory: &Inventory,
    ) -> Result<Self, String> {
        check_destination(&paths.daemon, inventory)?;
        let legacy = super::retained_binary::RetainedBinary::capture(
            old_destination,
            &paths.daemon,
            inventory,
        );
        let definition = if paths.definition.is_file() {
            let mut old_paths = paths.clone();
            old_paths.daemon = old_destination.to_path_buf();
            Some(runtime::DefinitionSnapshot::capture(
                RuntimePlatform::Macos,
                &old_paths,
            )?)
        } else {
            None
        };
        Ok(Self {
            replacement: None,
            paths: paths.clone(),
            definition,
            legacy,
        })
    }

    pub(super) fn attach(&mut self, replacement: Replacement) {
        self.replacement = Some(replacement);
    }

    pub(super) fn activate<R: CommandRunner>(
        &self,
        runner: &mut R,
        inventory: &mut Inventory,
        connection: Option<&Path>,
    ) -> Result<(), String> {
        let status = runtime::observe(RuntimePlatform::Macos, runner)?;
        if status.live {
            runtime::restart_with_definition_from_at(
                RuntimePlatform::Macos,
                &self.paths,
                &self.legacy.old,
                runner,
                inventory,
                connection,
            )?;
        } else {
            runtime::ensure(
                RuntimePlatform::Macos,
                &self.paths,
                status,
                runner,
                inventory,
            )?;
        }
        Ok(())
    }

    pub(super) fn rollback<R: CommandRunner>(
        &mut self,
        runner: &mut R,
        inventory: &mut Inventory,
        connection: Option<&Path>,
    ) -> Result<(), String> {
        if let Some(replacement) = &mut self.replacement {
            replacement.rollback()?;
        }
        self.replacement = None;
        self.legacy.restore(inventory);
        if let Some(definition) = &self.definition {
            definition.restore_from(runner, inventory, connection, &self.paths.daemon)?;
        } else {
            runtime::deregister(RuntimePlatform::Macos, &self.paths, None, runner)?;
            match fs::remove_file(&self.paths.definition) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            inventory.remove_owned_path("runtime-definition", &self.paths.definition);
            inventory.remove_owned_path("runtime-registration", &self.paths.definition);
        }
        inventory.save()
    }

    pub(super) fn complete(&mut self, inventory: &mut Inventory) -> Result<(), String> {
        if let Some(replacement) = &mut self.replacement {
            replacement.complete()?;
        }
        self.legacy.complete(inventory)?;
        inventory.save()
    }
}
