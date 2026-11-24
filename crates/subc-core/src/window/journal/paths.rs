use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use crate::setup::install_journal::{is_reparse_point, Journal};

pub(in crate::window) fn info(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error_at(path, error)),
    }
}

pub(in crate::window) fn error_at(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", path.display())
}

pub(in crate::window) fn plain_dir(path: &Path) -> Result<bool, String> {
    match info(path)? {
        Some(metadata) if metadata.is_dir() && !is_reparse_point(&metadata) => Ok(true),
        Some(_) => Err(error_at(path, "not a plain directory")),
        None => Ok(false),
    }
}

pub(in crate::window) fn normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some()
        && !path.components().any(|part| part == Component::ParentDir)
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(path).map_err(|error| error_at(path, error))?;
    if !normalized_absolute(&path) {
        return Err(error_at(
            &path,
            "placement paths must be UTF-8 and must not contain '..'",
        ));
    }
    Ok(path.components().collect())
}

pub(in crate::window) fn canonical(path: &Path) -> Result<PathBuf, String> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut existing) => {
                existing.extend(missing.into_iter().rev());
                return Ok(existing);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if info(cursor)?.is_some() {
                    return Err(error_at(cursor, "cannot resolve a dangling link"));
                }
                let (Some(parent), Some(name)) = (cursor.parent(), cursor.file_name()) else {
                    return Err(error_at(cursor, error));
                };
                missing.push(name);
                cursor = parent;
            }
            Err(error) => return Err(error_at(cursor, error)),
        }
    }
}

fn starts_with(path: &Path, base: &Path) -> bool {
    #[cfg(not(windows))]
    {
        path.starts_with(base)
    }
    #[cfg(windows)]
    {
        let key = |path: &Path| {
            path.components()
                .map(|part| part.as_os_str().to_string_lossy().to_uppercase())
                .collect::<Vec<_>>()
        };
        key(path).starts_with(&key(base))
    }
}

pub(in crate::window) fn same_path(a: &Path, b: &Path) -> bool {
    starts_with(a, b) && starts_with(b, a)
}

pub(in crate::window) fn overlaps(a: &Path, b: &Path) -> bool {
    starts_with(a, b) || starts_with(b, a)
}

pub(in crate::window) fn in_domain(root: &Path, path: &Path) -> Result<(), String> {
    for protected in [
        root.join("windows"),
        root.join("installer.lock"),
        root.join("stack.lock"),
        root.join("stack-stopped"),
        Journal::path(&root.join("installer-manifest.json")),
    ] {
        if overlaps(path, &protected) || overlaps(path, &canonical(&protected)?) {
            return Err(error_at(
                path,
                format!("overlaps protected path {}", protected.display()),
            ));
        }
    }
    Ok(())
}

pub(in crate::window) fn placement(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let raw = absolute(path)?;
    in_domain(root, &raw)?;
    if info(&raw)?.is_some_and(|info| is_reparse_point(&info)) {
        return Err(error_at(
            &raw,
            "placement path is a symlink or reparse point",
        ));
    }
    let resolved = canonical(&raw)?;
    in_domain(root, &resolved)?;
    if !normalized_absolute(&resolved) {
        return Err(error_at(&resolved, "resolved placement path is not UTF-8"));
    }
    Ok(resolved)
}
