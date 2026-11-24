use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::setup::install_journal::is_reparse_point;

use super::{
    files,
    journal::{
        self,
        paths::{self, error_at},
        Action, Role, Snapshot,
    },
    placement,
};

const FLOOR_NAME: &str = "format-floor.json";
const MAX_DEPTH: usize = 4;

pub(super) fn check(data: &Path, dir: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let mut checked = BTreeSet::new();
    for entry in &snapshot.steps {
        let step = &entry.step;
        let (Some(module), Some(Role::Binary)) = (&step.module, step.role) else {
            continue;
        };
        if !checked.insert((module, &step.target)) {
            continue;
        }
        let tree = data.join(module);
        let projected = projected_floors(&tree, dir, snapshot)?;
        if projected.is_empty() {
            continue;
        }
        let map = map_path(dir, snapshot, module, &step.target)?.ok_or_else(|| {
            error_at(
                &step.target,
                format!(
                    "{module} has format floors under {} but no format-versions map",
                    tree.display()
                ),
            )
        })?;
        let json = read_json(&map)?;
        let versions = json
            .as_object()
            .ok_or_else(|| error_at(&map, "format-versions map is not an object"))?;
        for (display, path) in projected {
            let floor = read_json(&path)?;
            let floor = floor
                .get("floors")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    error_at(&display, "invalid format floors: expected a floors object")
                })?;
            for (format, required) in floor {
                let required = integer(required).ok_or_else(|| {
                    error_at(&display, format!("floor for {format} is not an integer"))
                })?;
                let have = versions.get(format).and_then(integer);
                if have.is_none_or(|have| have < required) {
                    let have = have.map_or_else(|| "nothing".into(), |have| have.to_string());
                    return Err(error_at(&display, format!("{module}: {format} floor {required}, restored image reads {have} (map {})", map.display())));
                }
            }
        }
    }
    Ok(())
}

fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn map_path(
    dir: &Path,
    snapshot: &Snapshot,
    module: &str,
    binary: &Path,
) -> Result<Option<PathBuf>, String> {
    for (index, entry) in snapshot.steps.iter().enumerate() {
        let step = &entry.step;
        if step.module.as_deref() == Some(module) && step.role == Some(Role::FormatMap) {
            if entry.undone {
                return Ok(journal::content::file_info(&step.target)?.map(|_| step.target.clone()));
            }
            let (prior, _) = step
                .action
                .placement()
                .expect("only files have a format-map role");
            return Ok(prior.map(|_| placement::saved(dir, index + 1)));
        }
    }
    let primary = files::suffix(binary, ".format-versions.json");
    if journal::content::file_info(&primary)?.is_some() {
        return Ok(Some(primary));
    }
    let fallback = binary
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("SIGNED."))
        .map(|name| binary.with_file_name(format!("{name}.format-versions.json")));
    match fallback {
        Some(path) if journal::content::file_info(&path)?.is_some() => Ok(Some(path)),
        _ => Ok(None),
    }
}

fn projected_floors(
    tree: &Path,
    dir: &Path,
    snapshot: &Snapshot,
) -> Result<BTreeMap<PathBuf, PathBuf>, String> {
    let mut result = BTreeMap::new();
    let live = paths::info(tree)?;
    if live.as_ref().is_some_and(is_reparse_point) {
        return Err(error_at(tree, "module data tree is redirected"));
    }
    let tree = paths::canonical(tree)?;
    if live.is_some_and(|metadata| metadata.is_dir()) {
        // Do not traverse symlinks or reparse points. Every ordinary tree at
        // this depth matters, not just the directory containing a known store.
        let mut dirs = vec![(tree.clone(), 0)];
        while let Some((directory, depth)) = dirs.pop() {
            for path in journal::entries(&directory)? {
                let metadata =
                    paths::info(&path)?.ok_or_else(|| error_at(&path, "data entry disappeared"))?;
                if is_reparse_point(&metadata) {
                    continue;
                }
                if metadata.is_file() && path.file_name().is_some_and(|name| name == FLOOR_NAME) {
                    result.insert(path.clone(), path);
                } else if metadata.is_dir() && depth + 1 < MAX_DEPTH {
                    dirs.push((path, depth + 1));
                }
            }
        }
    }
    // Apply the same reverse order as undo. A store restore replaces the
    // entire floor inventory of its directory, including floors deleted since
    // it was recorded and floors whose live contents need not be readable.
    for (i, entry) in snapshot.steps.iter().enumerate().rev() {
        let Action::Store { floors, .. } = &entry.step.action else {
            continue;
        };
        if entry.undone {
            continue;
        }
        let store_tree = entry.step.target.parent().expect("store directory");
        result.retain(|path, _| !under(path, store_tree));
        let saved = placement::saved(dir, i + 1).join("floors");
        for floor in floors {
            let path = store_tree.join(&floor.path);
            if depth_under(&path, &tree).is_some_and(|depth| depth <= MAX_DEPTH) {
                result.insert(path, saved.join(&floor.path));
            }
        }
    }
    Ok(result)
}

fn under(path: &Path, top: &Path) -> bool {
    depth_under(path, top).is_some()
}

fn depth_under(path: &Path, top: &Path) -> Option<usize> {
    path.ancestors()
        .enumerate()
        .find_map(|(depth, ancestor)| paths::same_path(ancestor, top).then_some(depth))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let file = subc_os::open_regular_nofollow(path).map_err(|error| error_at(path, error))?;
    serde_json::from_reader(file).map_err(|error| error_at(path, error))
}
