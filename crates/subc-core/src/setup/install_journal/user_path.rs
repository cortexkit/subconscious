use super::{bootstrap, files, transaction::Transaction, Action, Step, Undo, UserPath};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use subc_os::user_path::{read_user_path, write_user_path, UserPathValue};

const TARGET: &str = r"HKCU\Environment\Path";

impl From<UserPathValue> for UserPath {
    fn from(value: UserPathValue) -> Self {
        let (expandable, text) = match value {
            UserPathValue::Literal(text) => (false, text),
            UserPathValue::Expandable(text) => (true, text),
        };
        Self {
            expandable,
            units: text.encode_wide().collect(),
        }
    }
}

fn set(value: Option<&UserPath>) -> Result<(), String> {
    let value = value.map(|value| {
        let text = OsString::from_wide(&value.units);
        if value.expandable {
            UserPathValue::Expandable(text)
        } else {
            UserPathValue::Literal(text)
        }
    });
    write_user_path(value.as_ref())
        .map_err(|error| format!("could not write {TARGET}: {error}"))?;
    subc_os::user_path::broadcast_user_environment_change();
    Ok(())
}

fn read() -> Result<Option<UserPath>, String> {
    read_user_path()
        .map(|value| value.map(Into::into))
        .map_err(|error| format!("could not read {TARGET}: {error}"))
}

pub(in crate::setup) fn from_bootstrap(manifest: &Path, running: &Path) -> Result<(), String> {
    if std::env::var_os("CK_BOOTSTRAP_WINDOWS_PATH").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(());
    }
    let root = manifest
        .parent()
        .ok_or("installer manifest has no parent")?;
    let ck = bootstrap::destination(root);
    if !running
        .canonicalize()
        .ok()
        .zip(ck.canonicalize().ok())
        .is_some_and(|(running, ck)| running == ck)
    {
        return Err(format!(
            "refusal: bootstrap PATH changes must run from {}",
            ck.display()
        ));
    }
    let cmd = ck.parent().ok_or("installed ck has no command directory")?;
    let mut inventory = bootstrap::read_inventory(manifest)?;
    let mut transaction = Transaction::begin(manifest)?;
    let result = (|| {
        let before = read()?;
        let after = Some(prepend(cmd, before.as_ref())?);
        if before != after {
            let index = transaction.journal().prepare(Step {
                target: PathBuf::from(TARGET),
                action: Action::UserPath { before, after },
            })?;
            replay(transaction.journal().step(index))?;
            transaction.journal().done(index)?;
        }
        inventory.record(
            "user-path-registry",
            cmd,
            Map::from_iter([
                (
                    "registry_key".into(),
                    Value::String(r"HKCU\Environment".into()),
                ),
                ("registry_value".into(), Value::String("Path".into())),
            ]),
        );
        let legacy = root.join("bin/ck.exe");
        if std::fs::symlink_metadata(root.join("bin"))
            .is_ok_and(|data| data.is_dir() && !super::is_reparse_point(&data))
            && std::fs::symlink_metadata(&legacy)
                .is_ok_and(|data| data.is_file() && !super::is_reparse_point(&data))
        {
            let old = files::digest(&legacy)?.ok_or("legacy ck.exe disappeared")?;
            let backup =
                legacy.with_file_name(format!("ck.exe.retired-{}", transaction.journal().id()));
            if std::fs::symlink_metadata(&backup).is_ok() {
                return Err(format!("refusal: {} already exists", backup.display()));
            }
            transaction.replace(&backup, &legacy)?;
            transaction.remove(&legacy)?;
            for kind in crate::setup::components::OWNED_BINARY_KINDS {
                inventory.remove_owned_path(kind, &legacy);
            }
            inventory.record(
                "legacy-cli-retirement",
                &backup,
                Map::from_iter([
                    (
                        "original_path".into(),
                        Value::String(legacy.to_string_lossy().into_owned()),
                    ),
                    (
                        "backup_path".into(),
                        Value::String(backup.to_string_lossy().into_owned()),
                    ),
                    ("sha256".into(), Value::String(old)),
                ]),
            );
        }
        Ok::<_, String>(())
    })();
    let mut recovery = bootstrap::FileRecovery;
    if let Err(error) = result {
        if let Err(undo) = transaction.undo(&mut recovery) {
            return Err(format!("{error}; install recovery stopped: {undo}"));
        }
        return Err(error);
    }
    transaction.commit(&inventory, &mut recovery)?;
    Ok(())
}

fn prepend(directory: &Path, old: Option<&UserPath>) -> Result<UserPath, String> {
    let command = directory
        .to_str()
        .ok_or("command directory is not Unicode")?;
    if command.contains([';', '"', '\0']) {
        return Err("command directory cannot be added to PATH".into());
    }
    let expandable = old.is_none_or(|old| old.expandable);
    if expandable && command.contains('%') {
        return Err(
            "command directory contains a percent sign that Windows would expand in PATH".into(),
        );
    }
    let normal = |value: &str| {
        value
            .trim()
            .trim_matches('"')
            .trim_end_matches(['/', '\\'])
            .replace('/', "\\")
            .to_lowercase()
    };
    let command_spelling = normal(command);
    let mut units: Vec<u16> = command.encode_utf16().collect();
    if let Some(old) = old {
        for entry in old.units.split(|&unit| unit == u16::from(b';')) {
            if normal(&String::from_utf16_lossy(entry)) == command_spelling {
                continue;
            }
            units.push(u16::from(b';'));
            units.extend_from_slice(entry);
        }
    }
    Ok(UserPath { expandable, units })
}

fn states(step: &Step) -> Result<(&Option<UserPath>, &Option<UserPath>), String> {
    let Action::UserPath { before, after } = &step.action else {
        return Err("expected a Windows PATH step".into());
    };
    if step.target != Path::new(TARGET) {
        return Err(format!(
            "refusal: unsupported registry path {}",
            step.target.display()
        ));
    }
    Ok((before, after))
}

pub(in crate::setup) fn verify(step: &Step) -> Result<bool, String> {
    Ok(&read()? == states(step)?.1)
}

pub(in crate::setup) fn can_replay(step: &Step) -> Result<bool, String> {
    Ok(&read()? == states(step)?.0)
}

pub(in crate::setup) fn replay(step: &Step) -> Result<(), String> {
    if !can_replay(step)? {
        return Err(format!("refusal: {TARGET} changed after write-ahead"));
    }
    set(states(step)?.1.as_ref())
}

pub(in crate::setup) fn undo(step: &Step, phase: Undo) -> Result<(), String> {
    if phase != Undo::Restore {
        return Ok(());
    }
    let current = read()?;
    let (before, after) = states(step)?;
    if should_restore(&current, before, after)? {
        set(before.as_ref())?;
    }
    Ok(())
}

fn should_restore(
    current: &Option<UserPath>,
    before: &Option<UserPath>,
    after: &Option<UserPath>,
) -> Result<bool, String> {
    if current == before {
        return Ok(false);
    }
    if current == after {
        return Ok(true);
    }
    let describe = |value: &Option<UserPath>| {
        value.as_ref().map_or_else(
            || "absent".into(),
            |path| {
                let units = String::from_utf16(&path.units).map_or_else(
                    |_| format!("UTF-16 units {:04x?}", path.units),
                    |text| format!("{text:?}"),
                );
                format!(
                    "{} {units}",
                    if path.expandable {
                        "REG_EXPAND_SZ"
                    } else {
                        "REG_SZ"
                    }
                )
            },
        )
    };
    Err(format!("refusal: {TARGET} has an unexpected value: current {}, recorded before {}, recorded after {}. Save the current Path elsewhere, use the Windows Environment Variables editor to restore either recorded value and type (or remove the value if recorded as absent), then rerun the installer command", describe(current), describe(before), describe(after)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_edit_preserves_registry_type_and_unrelated_entries() {
        let previous = UserPath::from(UserPathValue::Expandable(
            r"C:\tools;%USERPROFILE%\bin;c:/CK/;C:\space folder".into(),
        ));
        let result = prepend(Path::new(r"C:\ck"), Some(&previous)).unwrap();
        assert!(result.expandable);
        assert_eq!(
            String::from_utf16(&result.units).unwrap(),
            r"C:\ck;C:\tools;%USERPROFILE%\bin;C:\space folder"
        );
    }

    #[test]
    fn path_recovery_refusal_names_current_and_recorded_values_and_types() {
        let before = Some(UserPath::from(UserPathValue::Expandable(
            r"%USERPROFILE%\bin".into(),
        )));
        let after = Some(UserPath::from(UserPathValue::Literal(r"C:\tools".into())));
        let current = Some(UserPath::from(UserPathValue::Literal(
            r"C:\user-edit".into(),
        )));
        let error = should_restore(&current, &before, &after).unwrap_err();
        for expected in [
            TARGET,
            "current REG_SZ",
            "user-edit",
            "recorded before REG_EXPAND_SZ",
            "%USERPROFILE%",
            "recorded after REG_SZ",
            "tools",
            "rerun the installer command",
        ] {
            assert!(error.contains(expected), "missing {expected:?}: {error}");
        }
        assert!(should_restore(&after, &before, &after).unwrap());
        assert!(!should_restore(&before, &before, &after).unwrap());
    }
}
