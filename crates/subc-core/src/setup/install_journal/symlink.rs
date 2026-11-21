use super::{files, sync_parent, Action, FileStep, Journal, Step};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(in crate::setup) fn place(
    journal: &mut Journal,
    target: &Path,
    new: Option<&Path>,
) -> Result<(), String> {
    let old = link(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| message(target, "link needs a parent"))?;
    let staging_directory = parent.join(format!(
        ".ck-install-{}-{}",
        journal.id(),
        journal.next_index()
    ));
    let index = journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Symlink(FileStep {
            before: old.as_deref().map(path_digest),
            after: new.map(path_digest),
            previous: old.as_ref().map(|_| staging_directory.join("previous")),
            staged: new.map(|_| staging_directory.join("next")),
            staging_directory,
        }),
    })?;
    let step = journal.step(index);
    let record = step.action.file().unwrap();
    super::super::file_modes::ensure_private_parents(parent)?;
    files::create_stage(journal.id(), &record.staging_directory)?;
    if let (Some(old), Some(previous)) = (old, &record.previous) {
        std::os::unix::fs::symlink(old, previous).map_err(|error| message(previous, error))?;
        sync_parent(previous)?;
    }
    if let (Some(new), Some(staged)) = (new, &record.staged) {
        std::os::unix::fs::symlink(new, staged).map_err(|error| message(staged, error))?;
        sync_parent(staged)?;
    }
    replay(journal.id(), step)?;
    journal.done(index)
}

pub(in crate::setup) fn verify(step: &Step) -> Result<bool, String> {
    Ok(digest(&step.target)? == step.action.file().unwrap().after)
}

pub(in crate::setup) fn can_replay(id: &str, step: &Step) -> Result<bool, String> {
    let record = step.action.file().unwrap();
    if digest(&step.target)? != record.before || !files::owned_stage(id, &record.staging_directory)?
    {
        return Ok(false);
    }
    if let Some(next) = &record.staged {
        return Ok(digest(next)? == record.after);
    }
    Ok(true)
}

pub(in crate::setup) fn replay(id: &str, step: &Step) -> Result<(), String> {
    if !can_replay(id, step)? {
        return Err(message(
            &step.target,
            "link or staged target does not match the journal",
        ));
    }
    if let Some(next) = &step.action.file().unwrap().staged {
        replace(next, &step.target)
    } else {
        remove(&step.target)
    }
}

pub(in crate::setup) fn restore(id: &str, step: &Step) -> Result<(), String> {
    let record = step.action.file().unwrap();
    let actual = digest(&step.target)?;
    if actual == record.before {
        return Ok(());
    }
    if actual != record.after || !files::owned_stage(id, &record.staging_directory)? {
        return Err(message(
            &step.target,
            "link no longer matches the recorded replacement",
        ));
    }
    if let Some(previous) = &record.previous {
        if digest(previous)? != record.before {
            return Err(message(
                previous,
                "previous link no longer matches the journal",
            ));
        }
        return replace(previous, &step.target);
    }
    remove(&step.target)
}

fn replace(old: &Path, new: &Path) -> Result<(), String> {
    subc_os::durable_replace(old, new).map_err(|error| message(new, error))?;
    sync_parent(old)
}

fn remove(path: &Path) -> Result<(), String> {
    if link(path)?.is_some() {
        fs::remove_file(path).map_err(|error| message(path, error))?;
        sync_parent(path)?;
    }
    Ok(())
}

fn digest(path: &Path) -> Result<Option<String>, String> {
    Ok(link(path)?.as_deref().map(path_digest))
}

fn path_digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.as_os_str().as_bytes()))
}

fn link(path: &Path) -> Result<Option<PathBuf>, String> {
    match fs::read_link(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(message(path, error)),
    }
}

fn message(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("refusal: journaled symlink {}: {detail}", path.display())
}
