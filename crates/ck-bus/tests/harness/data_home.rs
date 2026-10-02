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

/// Files the operator's running ckbus rewrites in place while it serves. The
/// live module updates them whenever a module restarts or is issued a
/// credential, which can happen during a test run, so their contents are not
/// evidence about the test. Only their presence is compared. Each name is a
/// state-file constant in ck-bus's source (named beside it);
/// `every_ckbus_state_file_is_classified` fails if a new one is added there
/// without being listed here or in [`CONTENT_COMPARED`].
pub const LIVE_REWRITTEN: [&str; 4] = [
    "spawn_cursor.json",     // spawn_consumer::cursor::CURSOR_FILE
    "sentinel_verdict.json", // sentinel::verdict::VERDICT_FILE
    "own_users.json",        // bootstrap::store::OWN_USERS_FILE
    "epoch_high_water.json", // issuance::high_water::HIGH_WATER_FILE
];

/// Directories the live module or its supervised broker fills and empties while
/// serving. Revocation progress, resolver JWTs and JetStream data can change
/// during a run. Only the directory's presence is compared; its children are
/// not recorded.
pub const LIVE_SUBTREES: [&str; 3] = [
    "revocation_progress", // revocation::progress::PROGRESS_DIR
    "jwt",                 // install::JWT_DIR (live broker resolver)
    "js",                  // install::JS_DIR (live broker JetStream store)
];

/// State files and trees not rewritten while serving, so any change to their
/// content during a run is evidence against the test. Install artifacts are
/// written only by the offline install commands, not by the live module.
pub const CONTENT_COMPARED: [&str; 5] = [
    "account.json",   // bootstrap::store::ACCOUNT_FILE
    "system_account", // install::SYSTEM_ACCOUNT_FILE
    "operator.jwt",   // install::OPERATOR_JWT_FILE
    "server.conf",    // install::SERVER_CONF_FILE
    "ceremony",       // install::CEREMONY_DIR
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
    if relative.parent() == Some(Path::new("")) {
        let name = relative.to_string_lossy();
        if metadata.is_file() && LIVE_REWRITTEN.contains(&name.as_ref()) {
            entries.insert(relative, "file:live-rewritten".to_string());
            return;
        }
        if metadata.is_dir() && LIVE_SUBTREES.contains(&name.as_ref()) {
            entries.insert(relative, "dir:live-subtree".to_string());
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
