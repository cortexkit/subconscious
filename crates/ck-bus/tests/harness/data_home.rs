use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFingerprint(BTreeMap<PathBuf, String>);

pub fn operator_module_dir() -> Option<PathBuf> {
    let root = env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(root.join("cortexkit/ckbus"))
}

/// Files the operator's running ckbus rewrites in place while it serves: its
/// cursor on the daemon's spawn stream, its bus sentinel's verdict and its own
/// user list. The live module updates them whenever a module restarts, which
/// can happen during a test run, so their contents are not evidence about the
/// test. Only their presence is compared. Everything else (the machine account,
/// and any file a test might wrongly create) is compared by content.
const LIVE_REWRITTEN: [&str; 3] = [
    "spawn_cursor.json",
    "sentinel_verdict.json",
    "own_users.json",
];

pub fn fingerprint(root: Option<&Path>) -> TreeFingerprint {
    let mut entries = BTreeMap::new();
    if let Some(root) = root {
        fingerprint_path(root, root, &mut entries);
    }
    TreeFingerprint(entries)
}

fn fingerprint_path(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, String>) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let kind = if metadata.is_dir() {
        "dir"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "file"
    };
    if metadata.is_file() && relative.parent() == Some(Path::new("")) {
        let name = relative.to_string_lossy();
        if LIVE_REWRITTEN.contains(&name.as_ref()) {
            entries.insert(relative, "file:live-rewritten".to_string());
            return;
        }
    }
    let digest = if metadata.is_file() {
        fs::read(path)
            .ok()
            .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
            .unwrap_or_else(|| "unreadable".to_string())
    } else {
        String::new()
    };
    // A directory is recorded by presence only: its own size and modification
    // time change whenever the live module replaces a file inside it by rename,
    // while every child is recorded as its own entry, so an added or removed
    // file is still caught.
    let entry = if metadata.is_dir() {
        "dir".to_string()
    } else {
        format!("{kind}:{}:{modified}:{digest}", metadata.len())
    };
    entries.insert(relative, entry);
    if metadata.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)
            .unwrap_or_else(|error| {
                panic!(
                    "operator data-home observation failed at {}: {error}",
                    path.display()
                )
            })
            .map(|entry| {
                entry
                    .expect("operator data-home directory entry must be readable")
                    .path()
            })
            .collect();
        children.sort();
        for child in children {
            fingerprint_path(root, &child, entries);
        }
    }
}
