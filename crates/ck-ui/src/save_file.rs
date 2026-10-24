//! Replace only reviewed configuration bytes, retaining a backup.
//! Backups are owner-only (0600) on Unix; on Windows a backup inherits the
//! config folder's access.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

#[derive(Debug)]
pub enum WriteError {
    Changed(String),
    Failed(String),
}

#[cfg(test)]
thread_local! {
    static AFTER_TEMP_SYNC: std::cell::Cell<Option<fn(&Path)>> = const { std::cell::Cell::new(None) };
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Resolving the target, rather than renaming over the reported path, preserves
/// a symlink. Exclusive creation reserves backup names even for simultaneous saves.
pub fn write(path: &Path, original: &str, candidate: &str) -> Result<PathBuf, WriteError> {
    write_at(
        path,
        original,
        candidate,
        &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
    )
}

pub fn write_at(
    path: &Path,
    original: &str,
    candidate: &str,
    stamp: &str,
) -> Result<PathBuf, WriteError> {
    let failed = |e: std::io::Error| WriteError::Failed(e.to_string());
    let target = fs::canonicalize(path).map_err(failed)?;
    let check = || {
        let disk = fs::read_to_string(path).map_err(failed)?;
        if disk != original || fs::canonicalize(path).map_err(failed)? != target {
            Err(WriteError::Changed(disk))
        } else {
            Ok(())
        }
    };
    check()?;
    let permissions = fs::metadata(&target).map_err(failed)?.permissions();
    let name = target.file_name().unwrap().to_string_lossy();
    let mut n = 1;
    let (backup, mut file) = loop {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let backup = target.with_file_name(format!("{name}.bak-{stamp}{suffix}"));
        match private_file(&backup) {
            Ok(file) => break (backup, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(failed(e)),
        }
    };
    file.write_all(original.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    drop(file);
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let (temp, mut file) = loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = target.with_file_name(format!(".{name}.ck-{}-{id}", std::process::id()));
        match private_file(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(failed(e)),
        }
    };
    let result = (|| {
        file.write_all(candidate.as_bytes()).map_err(failed)?;
        file.set_permissions(permissions).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        drop(file);
        #[cfg(test)]
        AFTER_TEMP_SYNC.with(|hook| {
            if let Some(hook) = hook.take() {
                hook(path);
            }
        });
        // Check again after preparing the replacement: another writer may have
        // changed the file while the backup or temp file was being written.
        check()?;
        fs::rename(&temp, &target).map_err(failed)?;
        Ok(backup)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub path: Vec<String>,
    pub disk: Option<Value>,
}

fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str(&subc_jsonc::jsonc_to_json(text)?).map_err(|e| e.to_string())
}

fn value_at<'a>(mut value: &'a Value, path: &[String]) -> Option<&'a Value> {
    for key in path {
        value = match value {
            Value::Array(array) => array.get(key.parse::<usize>().ok()?)?,
            _ => value.get(key)?,
        };
    }
    Some(value)
}

pub fn replace(text: &mut String, path: &[String], value: Option<&Value>) -> Result<(), String> {
    replace_with(text, path, value, |_| {})
}

pub(crate) fn replace_with(
    text: &mut String,
    path: &[String],
    value: Option<&Value>,
    after_edit: impl FnOnce(&mut String),
) -> Result<(), String> {
    let before = parse(text)?;
    let expected = expected_replacement(&before, path, value).ok_or_else(|| {
        format!(
            "Couldn't drop your edit ({}); the conflict is still open",
            setting(path)
        )
    })?;
    let mut replacement = text.clone();
    replace_unchecked(&mut replacement, path, value)?;
    after_edit(&mut replacement);
    verify_expected(&expected, &replacement, path).map_err(|setting| {
        format!("Couldn't drop your edit ({setting}); the conflict is still open")
    })?;
    *text = replacement;
    Ok(())
}

fn replace_unchecked(
    text: &mut String,
    path: &[String],
    value: Option<&Value>,
) -> Result<(), String> {
    let refs: Vec<&str> = path.iter().map(String::as_str).collect();
    let document = parse(text)?;
    if let Some((last, parent)) = path.split_last() {
        if value_at(&document, parent).is_some_and(Value::is_array) {
            let index = last.parse::<usize>().map_err(|e| e.to_string())?;
            return match value {
                Some(value) => subc_jsonc::set_element(text, &refs[..refs.len() - 1], index, value),
                None => subc_jsonc::remove_element(text, &refs[..refs.len() - 1], index),
            };
        }
    }
    if let (Some(Value::Array(old)), Some(Value::Array(new))) = (value_at(&document, path), value) {
        for index in (new.len()..old.len()).rev() {
            subc_jsonc::remove_element(text, &refs, index)?;
        }
        for (index, value) in new.iter().enumerate() {
            if index >= old.len() {
                subc_jsonc::push_element(text, &refs, value)?;
            } else if old[index] != *value {
                subc_jsonc::set_element(text, &refs, index, value)?;
            }
        }
        return Ok(());
    }
    match value {
        Some(value) => subc_jsonc::set_member(text, &refs, value),
        None => subc_jsonc::remove_member(text, &refs).map(|_| ()),
    }
}

fn setting(path: &[String]) -> String {
    if path.is_empty() {
        "configuration".into()
    } else {
        path.join(".")
    }
}

/// The expected value is built with plain JSON operations, independently of
/// the span editor, so a text edit cannot validate its own misplaced result.
fn expected_replacement(before: &Value, path: &[String], value: Option<&Value>) -> Option<Value> {
    use crate::screens::settings::Edit;
    if path.is_empty() {
        return value.cloned();
    }
    if value.is_none() && value_at(before, path).is_none() {
        return Some(before.clone());
    }
    let (last, parent) = path.split_last()?;
    let edit = if value_at(before, parent).is_some_and(Value::is_array) {
        let index = last.parse().ok()?;
        match value {
            Some(value) => Edit::SetElement {
                path: parent.to_vec(),
                index,
                value: value.clone(),
            },
            None => Edit::RemoveElement {
                path: parent.to_vec(),
                index,
            },
        }
    } else {
        match value {
            Some(value) => Edit::Set {
                path: path.to_vec(),
                value: value.clone(),
            },
            None => Edit::Remove {
                path: path.to_vec(),
            },
        }
    };
    edit.expected(before)
}

fn first_difference(
    expected: Option<&Value>,
    actual: Option<&Value>,
    path: &mut Vec<String>,
) -> Option<String> {
    if expected == actual {
        return None;
    }
    match (expected, actual) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                path.push(key.clone());
                let difference = first_difference(a.get(key), b.get(key), path);
                path.pop();
                if difference.is_some() {
                    return difference;
                }
            }
        }
        (Some(Value::Array(a)), Some(Value::Array(b))) => {
            for index in 0..a.len().max(b.len()) {
                path.push(index.to_string());
                let difference = first_difference(a.get(index), b.get(index), path);
                path.pop();
                if difference.is_some() {
                    return difference;
                }
            }
        }
        _ => return Some(setting(path)),
    }
    None
}

fn verify_expected(expected: &Value, text: &str, fallback: &[String]) -> Result<(), String> {
    let actual = parse(text).map_err(|_| setting(fallback))?;
    match first_difference(Some(expected), Some(&actual), &mut Vec::new()) {
        Some(setting) => Err(setting),
        None => Ok(()),
    }
}

/// Reapply changed values to the new text, not to a reserialized document, so
/// unrelated comments and formatting from the other writer survive.
pub fn rebase(
    original: &str,
    candidate: &str,
    disk: &str,
) -> Result<(String, Vec<Conflict>), String> {
    rebase_with(original, candidate, disk, |_| {})
}

pub(crate) fn rebase_with(
    original: &str,
    candidate: &str,
    disk: &str,
    after_edit: impl FnOnce(&mut String),
) -> Result<(String, Vec<Conflict>), String> {
    fn changes(before: &Value, after: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        if before == after {
            return;
        }
        if let (Some(a), Some(b)) = (before.as_object(), after.as_object()) {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                path.push(key.clone());
                match (a.get(key), b.get(key)) {
                    (Some(a), Some(b)) => changes(a, b, path, out),
                    (None, Some(Value::Object(b))) if !b.is_empty() => changes(
                        &Value::Object(Default::default()),
                        &Value::Object(b.clone()),
                        path,
                        out,
                    ),
                    _ => out.push(path.clone()),
                }
                path.pop();
            }
        } else if let (Some(a), Some(b)) = (before.as_array(), after.as_array()) {
            if a.len() == b.len() {
                for (index, (a, b)) in a.iter().zip(b).enumerate() {
                    path.push(index.to_string());
                    changes(a, b, path, out);
                    path.pop();
                }
            } else {
                out.push(path.clone());
            }
        } else {
            out.push(path.clone());
        }
    }
    let (before, after, on_disk) = (parse(original)?, parse(candidate)?, parse(disk)?);
    let mut paths = Vec::new();
    changes(&before, &after, &mut Vec::new(), &mut paths);
    for path in &mut paths {
        // A removed or retyped parent is itself the conflicting value. Keeping
        // the edit restores the candidate's whole parent; dropping it restores
        // the parent's value from disk, or its absence if it was removed.
        for depth in 1..path.len() {
            let old = value_at(&before, &path[..depth]);
            let disk = value_at(&on_disk, &path[..depth]);
            if old.is_some_and(|old| {
                (old.is_object() || old.is_array())
                    && disk.is_none_or(|disk| {
                        old.is_object() != disk.is_object() || old.is_array() != disk.is_array()
                    })
            }) {
                path.truncate(depth);
                break;
            }
        }
    }
    paths.sort();
    paths.dedup();
    let mut text = disk.to_string();
    let mut expected = on_disk.clone();
    let mut conflicts = Vec::new();
    for path in &paths {
        let old = value_at(&before, path);
        let mine = value_at(&after, path);
        let theirs = value_at(&on_disk, path);
        if theirs != old && theirs != mine {
            conflicts.push(Conflict {
                path: path.clone(),
                disk: theirs.cloned(),
            });
        }
        expected = expected_replacement(&expected, path, mine).ok_or_else(|| {
            format!(
                "Couldn't rebase your edits onto the changed file ({}); nothing was saved",
                setting(path)
            )
        })?;
        replace_unchecked(&mut text, path, mine)?;
    }
    after_edit(&mut text);
    verify_expected(
        &expected,
        &text,
        paths.first().map_or(&[][..], Vec::as_slice),
    )
    .map_err(|setting| {
        format!("Couldn't rebase your edits onto the changed file ({setting}); nothing was saved")
    })?;
    Ok((text, conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_at_refuses_a_file_changed_after_the_temp_is_synced() {
        const ORIGINAL: &str = "{\"modules\":{}}\n";
        const CANDIDATE: &str = "{\"modules\":{\"broca\":{\"enabled\":false}}}\n";
        const OTHER_WRITER: &str = "// another writer's comment\n{\"modules\":{}}\n";
        let root = cortexkit_test_support::ScratchDir::new("settings-write-second-check");
        let path = root.join("subc.jsonc");
        fs::write(&path, ORIGINAL).unwrap();
        AFTER_TEMP_SYNC.with(|hook| {
            assert!(hook
                .replace(Some(|path| {
                    let temps: Vec<_> = fs::read_dir(path.parent().unwrap())
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .filter(|entry| {
                            entry
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .contains(".ck-")
                        })
                        .collect();
                    assert_eq!(temps.len(), 1, "the replacement is already prepared");
                    assert_eq!(fs::read(&temps[0]).unwrap(), CANDIDATE.as_bytes());
                    fs::write(path, OTHER_WRITER).unwrap();
                }))
                .is_none());
        });

        let result = write_at(&path, ORIGINAL, CANDIDATE, "20260102T030405Z");

        assert!(
            matches!(result, Err(WriteError::Changed(ref text)) if text == OTHER_WRITER),
            "{result:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), OTHER_WRITER.as_bytes());
        let files: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(
            files
                .iter()
                .all(|path| !path.file_name().unwrap().to_string_lossy().contains(".ck-")),
            "the refused replacement temp must be removed"
        );
        assert_eq!(
            files.len(),
            2,
            "only the other writer's file and backup remain"
        );
        assert_eq!(
            fs::read(root.join("subc.jsonc.bak-20260102T030405Z")).unwrap(),
            ORIGINAL.as_bytes()
        );
    }
}
