use super::{files, is_reparse_point, sync_parent, Action, FileStep, Journal, Step};
use crate::setup::file_modes;
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

/// Journal a flat directory of release-owned files, not a user data tree.
pub(in crate::setup) fn place(
    journal: &mut Journal,
    target: &Path,
    candidate: &Path,
    before: Option<String>,
) -> Result<String, String> {
    let after = digest(candidate)?.ok_or("missing staged directory")?;
    if digest(target)? != before {
        return Err(error(target, "directory changed before placement"));
    }
    if before.as_deref() == Some(after.as_str()) {
        return Ok(after);
    }
    let parent = target.parent().ok_or("directory needs a parent")?;
    let staging_directory = parent.join(format!(
        ".ck-install-{}-{}",
        journal.id(),
        journal.next_index()
    ));
    // Creating the parent does not publish any release files.
    file_modes::ensure_private_parents(parent)?;
    let index = journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Directory(FileStep {
            before,
            after: Some(after.clone()),
            previous: Some(staging_directory.join("previous")),
            staged: Some(staging_directory.join("next")),
            staging_directory,
        }),
    })?;
    let step = journal.step(index);
    let file = step.action.file().unwrap();
    files::create_stage(journal.id(), &file.staging_directory)?;
    let next = file.staged.as_deref().unwrap();
    file_modes::create_private_directory(next)?;
    for entry in fs::read_dir(candidate).map_err(|detail| error(candidate, detail))? {
        let entry = entry.map_err(|detail| error(candidate, detail))?;
        let to = next.join(entry.file_name());
        file_modes::copy_file_with_mode(&entry.path(), &to, 0o600)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&to)
            .and_then(|f| f.sync_all())
            .map_err(|detail| error(&to, detail))?;
    }
    sync_parent(&next.join("entry"))?;
    sync_parent(next)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("directory-staged");
    replay(journal.id(), step)?;
    journal.done(index)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("directory-done");
    Ok(after)
}

pub(super) fn is_applied(step: &Step) -> Result<bool, String> {
    Ok(digest(&step.target)? == step.action.file().unwrap().after)
}

pub(super) fn can_replay(id: &str, step: &Step) -> Result<bool, String> {
    let file = step.action.file().unwrap();
    if !files::owned_stage(id, &file.staging_directory)? {
        return Ok(false);
    }
    let now = digest(&step.target)?;
    let previous = digest(file.previous.as_deref().unwrap())?;
    Ok(digest(file.staged.as_deref().unwrap())? == file.after
        && ((now == file.before && previous.is_none())
            || (now.is_none() && file.before.is_some() && previous == file.before)))
}

pub(super) fn replay(id: &str, step: &Step) -> Result<(), String> {
    if !can_replay(id, step)? {
        return Err(error(&step.target, "cannot publish recorded directory"));
    }
    let file = step.action.file().unwrap();
    let next = file.staged.as_deref().unwrap();
    if digest(&step.target)?.is_some() {
        #[cfg(unix)]
        {
            exchange(next, &step.target)?;
            #[cfg(feature = "test-support")]
            super::crash_hook("directory-published");
            move_tree(next, file.previous.as_deref().unwrap())?;
            #[cfg(feature = "test-support")]
            super::crash_hook("directory-previous-retained");
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            move_tree(&step.target, file.previous.as_deref().unwrap())?;
            #[cfg(feature = "test-support")]
            super::crash_hook("directory-previous-retained");
        }
    }
    move_tree(next, &step.target)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("directory-published");
    Ok(())
}

pub(super) fn undo(id: &str, step: &Step) -> Result<(), String> {
    let file = step.action.file().unwrap();
    let actual = digest(&step.target)?;
    if actual == file.before {
        return Ok(());
    }
    if (actual.is_some() && actual != file.after)
        || !files::owned_stage(id, &file.staging_directory)?
    {
        return Err(error(
            &step.target,
            "directory no longer matches the journal",
        ));
    }
    let undone = file.staging_directory.join("undone");
    if file.before.is_none() {
        return move_tree(&step.target, &undone);
    }
    let previous = file.previous.as_deref().unwrap();
    // A crash immediately after an exchange leaves the old tree at `next`.
    let retained = if digest(previous)? == file.before {
        previous
    } else {
        file.staged.as_deref().unwrap()
    };
    if digest(retained)? != file.before {
        return Err(error(retained, "missing recorded rollback directory"));
    }
    if actual.is_some() {
        #[cfg(unix)]
        return exchange(retained, &step.target);
        #[cfg(not(unix))]
        move_tree(&step.target, &undone)?;
    }
    move_tree(retained, &step.target)
}

#[cfg(unix)]
fn exchange(from: &Path, to: &Path) -> Result<(), String> {
    crate::setup::daemon_bundle::exchange(from, to).map_err(|detail| error(to, detail))?;
    sync_parent(to)?;
    sync_parent(from)
}

fn move_tree(from: &Path, to: &Path) -> Result<(), String> {
    if metadata(to)?.is_some() {
        return Err(error(to, "directory destination is occupied"));
    }
    fs::rename(from, to).map_err(|detail| error(to, detail))?;
    sync_parent(to)?;
    sync_parent(from)
}

pub(in crate::setup) fn digest(root: &Path) -> Result<Option<String>, String> {
    let Some(info) = metadata(root)? else {
        return Ok(None);
    };
    if !info.is_dir() || is_reparse_point(&info) {
        return Err(error(root, "not a plain directory"));
    }
    let mut entries = fs::read_dir(root)
        .and_then(|entries| entries.collect::<io::Result<Vec<_>>>())
        .map_err(|detail| error(root, detail))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    let mut hash = Sha256::new();
    for entry in entries {
        let path = entry.path();
        if !metadata(&path)?.is_some_and(|info| info.is_file() && !is_reparse_point(&info)) {
            return Err(error(&path, "not a plain file"));
        }
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        hash.update(files::digest(&path)?.ok_or_else(|| error(&path, "file disappeared"))?);
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(detail) => Err(error(path, detail)),
    }
}

fn error(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("refusal: journaled directory {}: {detail}", path.display())
}

#[cfg(all(test, feature = "test-support"))]
mod tests;
