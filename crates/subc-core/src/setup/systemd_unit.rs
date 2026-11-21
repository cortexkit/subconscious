//! Where `ck setup` writes the daemon's systemd user unit, and the check that
//! the user manager loads it from there.
//!
//! The user manager searches `$XDG_CONFIG_HOME/systemd/user` when that
//! variable is set to an absolute path and `$HOME/.config/systemd/user`
//! otherwise; a unit written anywhere else is never found by name.

use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

use super::{inventory::Inventory, runtime::CommandRunner};

pub(super) const UNIT_NAME: &str = "cortexkit-subc.service";

/// The user unit directory the systemd user manager searches for this
/// session's configuration.
pub(super) fn unit_directory(user_home: &Path, xdg_config_home: Option<&OsStr>) -> PathBuf {
    match xdg_config_home.map(Path::new) {
        Some(config_home) if config_home.is_absolute() => config_home.join("systemd").join("user"),
        _ => user_home.join(".config").join("systemd").join("user"),
    }
}

/// The unit path for this process's environment.
pub(super) fn session_unit_path(user_home: &Path) -> PathBuf {
    unit_directory(user_home, std::env::var_os("XDG_CONFIG_HOME").as_deref()).join(UNIT_NAME)
}

/// The file the user manager says it loads the unit from: `Ok(None)` when it
/// names none (the unit is not in its search path). A `systemctl show` that
/// fails is an error: what the name means is then unknown.
pub(super) fn loaded_fragment<R: CommandRunner + ?Sized>(
    runner: &mut R,
) -> Result<Option<PathBuf>, String> {
    let args = [
        "--user".to_string(),
        "show".to_string(),
        "-p".to_string(),
        "FragmentPath".to_string(),
        UNIT_NAME.to_string(),
    ];
    let result = runner.run("systemctl", &args)?;
    if !result.success {
        return Err(format!(
            "refusal: could not ask the systemd user manager which file it loads {UNIT_NAME} from (systemctl said: {}); ck does not act on the unit by name without knowing",
            result.explanation()
        ));
    }
    Ok(result
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("FragmentPath="))
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .map(PathBuf::from))
}

fn same_file(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

/// Refuses unless the user manager names `definition` as the file it loads
/// the unit from. Used after a reload, before anything is done to the unit
/// by name.
pub(super) fn verify_fragment<R: CommandRunner + ?Sized>(
    definition: &Path,
    runner: &mut R,
) -> Result<(), String> {
    match loaded_fragment(runner)? {
        Some(fragment) if same_file(&fragment, definition) => Ok(()),
        loaded => Err(format!(
            "refusal: setup's {UNIT_NAME} is {}, but the systemd user manager loads it from {}; check XDG_CONFIG_HOME for the user manager (`systemctl --user show-environment`) and run ck setup again from a shell with the same value",
            definition.display(),
            loaded.map_or_else(
                || "no file (the unit was not found in its search path)".to_string(),
                |fragment| fragment.display().to_string()
            )
        )),
    }
}

/// For observation: refuses when the user manager loads the unit from a
/// file other than setup's `definition` or one of `also_setups` (setup's
/// own unit recorded at the other configuration root, which a migration is
/// moving). A manager that loads no file is left to registration, which
/// verifies the fragment after its reload.
pub(super) fn refuse_foreign_fragment<R: CommandRunner + ?Sized>(
    definition: &Path,
    also_setups: &[PathBuf],
    runner: &mut R,
) -> Result<(), String> {
    let Some(fragment) = loaded_fragment(runner)? else {
        return Ok(());
    };
    if same_file(&fragment, definition) || also_setups.iter().any(|path| same_file(&fragment, path))
    {
        return Ok(());
    }
    Err(format!(
        "refusal: the systemd user manager loads {UNIT_NAME} from {}, not from setup's {}; that unit is not setup's, so ck will not start, restart or remove it. Remove or rename it (or fix XDG_CONFIG_HOME for the user manager) and run ck setup again",
        fragment.display(),
        definition.display()
    ))
}

/// Setup's unit recorded at a path other than `definition`: the other of
/// `$XDG_CONFIG_HOME` and `$HOME/.config`.
pub(super) fn recorded_elsewhere(definition: &Path, inventory: &Inventory) -> Option<PathBuf> {
    inventory
        .paths_for_kind("runtime-definition")
        .into_iter()
        .find(|path| path != definition && path.file_name() == Some(OsStr::new(UNIT_NAME)))
}

/// The contents of a regular file (never through a link), or `None` when
/// nothing is there.
pub(super) fn regular_contents(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => fs::read(path)
            .map(Some)
            .map_err(|error| format!("could not read {}: {error}", path.display())),
        Ok(_) => Err(format!(
            "refusal: {} is not a regular file; setup does not move or replace it",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
    }
}
