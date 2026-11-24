use super::{files, sync_parent, Action, FileStep, Journal, Step};
use crate::setup::{daemon_bundle, runtime::CommandRunner};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(in crate::setup) fn place(
    journal: &mut Journal,
    target: &Path,
    candidate: &Path,
    runner: &mut impl CommandRunner,
) -> Result<(), String> {
    daemon_bundle::verify_bundle(candidate, runner)?;
    let before = tree_digest(target)?;
    let after = tree_digest(candidate)?.ok_or_else(|| error(candidate, "bundle is missing"))?;
    if before.is_some() {
        daemon_bundle::verify_bundle(target, runner)?;
    }
    let parent = target
        .parent()
        .ok_or_else(|| error(target, "bundle needs a parent directory"))?;
    let staging_directory = parent.join(format!(
        ".ck-install-{}-{}",
        journal.id(),
        journal.next_index()
    ));
    let index = journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Bundle(FileStep {
            before,
            after: Some(after),
            previous: Some(staging_directory.join("previous")),
            staged: Some(staging_directory.join("next")),
            staging_directory,
        }),
    })?;
    let step = journal.step(index);
    let file = step.action.file().unwrap();
    super::super::file_modes::ensure_private_parents(parent)?;
    files::create_stage(journal.id(), &file.staging_directory)?;
    let staged = file.staged.as_ref().unwrap();
    let result = runner.run(
        "ditto",
        &[
            candidate.to_string_lossy().into_owned(),
            staged.to_string_lossy().into_owned(),
        ],
    )?;
    if !result.success {
        return Err(error(staged, result.explanation()));
    }
    daemon_bundle::verify_bundle(staged, runner)?;
    sync_tree(staged)?;
    publish(step)?;
    journal.done(index)
}

fn publish(step: &Step) -> Result<(), String> {
    let file = step.action.file().unwrap();
    let staged = file.staged.as_deref().unwrap();
    let previous = file.previous.as_deref().unwrap();
    verify_digest(&step.target, &file.before)?;
    verify_digest(staged, &file.after)?;
    if file.before.is_some() {
        exchange(staged, &step.target)?;
        #[cfg(test)]
        tests::after_exchange();
        move_bundle(staged, previous)?;
    } else {
        move_bundle(staged, &step.target)?;
    }
    Ok(())
}

pub(in crate::setup) fn remove(
    journal: &mut Journal,
    target: &Path,
    runner: &mut impl CommandRunner,
) -> Result<(), String> {
    daemon_bundle::verify_bundle(target, runner)?;
    let before = tree_digest(target)?.ok_or_else(|| error(target, "bundle is missing"))?;
    let parent = target
        .parent()
        .ok_or_else(|| error(target, "bundle needs a parent directory"))?;
    let staging_directory = parent.join(format!(
        ".ck-install-{}-{}",
        journal.id(),
        journal.next_index()
    ));
    let index = journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Bundle(FileStep {
            before: Some(before),
            after: None,
            previous: Some(staging_directory.join("previous")),
            staged: None,
            staging_directory,
        }),
    })?;
    let step = journal.step(index);
    let file = step.action.file().unwrap();
    files::create_stage(journal.id(), &file.staging_directory)?;
    replay(journal.id(), step, runner)?;
    journal.done(index)
}

pub(in crate::setup) fn is_applied(
    step: &Step,
    runner: &mut impl CommandRunner,
) -> Result<bool, String> {
    let file = step.action.file().unwrap();
    if tree_digest(&step.target)? != file.after {
        return Ok(false);
    }
    if file.after.is_none() {
        return Ok(true);
    }
    daemon_bundle::verify_bundle(&step.target, runner)?;
    Ok(true)
}

pub(in crate::setup) fn can_replay(id: &str, step: &Step) -> Result<bool, String> {
    let file = step.action.file().unwrap();
    if tree_digest(&step.target)? != file.before
        || !files::owned_stage(id, &file.staging_directory)?
    {
        return Ok(false);
    }
    if let Some(staged) = &file.staged {
        return Ok(tree_digest(staged)? == file.after);
    }
    Ok(file.after.is_none())
}

pub(in crate::setup) fn replay(
    id: &str,
    step: &Step,
    runner: &mut impl CommandRunner,
) -> Result<(), String> {
    if !can_replay(id, step)? {
        return Err(error(
            &step.target,
            "cannot replay bundle publication from the recorded state",
        ));
    }
    let file = step.action.file().unwrap();
    if file.after.is_none() {
        daemon_bundle::verify_bundle(&step.target, runner)?;
        return move_bundle(
            &step.target,
            file.previous
                .as_deref()
                .ok_or_else(|| error(&step.target, "missing staging location"))?,
        );
    }
    daemon_bundle::verify_bundle(
        step.action.file().unwrap().staged.as_deref().unwrap(),
        runner,
    )?;
    publish(step)
}

pub(in crate::setup) fn undo(
    id: &str,
    step: &Step,
    runner: &mut impl CommandRunner,
) -> Result<(), String> {
    let file = step.action.file().unwrap();
    let actual = tree_digest(&step.target)?;
    if actual == file.before {
        return Ok(());
    }
    if actual != file.after {
        return Err(error(
            &step.target,
            "bundle no longer matches the journal's replacement",
        ));
    }
    if !files::owned_stage(id, &file.staging_directory)? {
        return Err(error(
            &file.staging_directory,
            "bundle recovery data are missing",
        ));
    }
    if file.before.is_none() {
        return move_bundle(&step.target, &file.staging_directory.join("undone"));
    }
    let previous = file.previous.as_deref().unwrap();
    // The previous tree is still at the exchange path if the process died
    // between the swap and giving the rollback its final staging name.
    let retained = if tree_digest(previous)?.is_some() {
        previous
    } else {
        file.staged.as_deref().unwrap()
    };
    verify_digest(retained, &file.before)?;
    daemon_bundle::verify_bundle(retained, runner)?;
    if actual.is_none() {
        return move_bundle(retained, &step.target);
    }
    exchange(retained, &step.target)
}

fn exchange(from: &Path, to: &Path) -> Result<(), String> {
    daemon_bundle::exchange(from, to).map_err(|detail| error(to, detail))?;
    sync_parent(to)?;
    sync_parent(from)
}

fn move_bundle(from: &Path, to: &Path) -> Result<(), String> {
    if metadata(to)?.is_some() {
        return Err(error(to, "bundle would overwrite another staging tree"));
    }
    fs::rename(from, to).map_err(|detail| error(to, detail))?;
    sync_parent(to)?;
    sync_parent(from)
}

fn verify_digest(path: &Path, expected: &Option<String>) -> Result<(), String> {
    if tree_digest(path)? != *expected {
        return Err(error(
            path,
            "bundle contents do not match the recorded digest",
        ));
    }
    Ok(())
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(detail) => Err(error(path, detail)),
    }
}

pub(crate) fn tree_digest(root: &Path) -> Result<Option<String>, String> {
    let Some(info) = metadata(root)? else {
        return Ok(None);
    };
    if !info.is_dir() {
        return Err(error(root, "not a plain bundle directory"));
    }
    let mut digest = Sha256::new();
    hash_tree(root, root, &mut digest)?;
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn hash_tree(root: &Path, path: &Path, digest: &mut Sha256) -> Result<(), String> {
    let info = fs::symlink_metadata(path).map_err(|detail| error(path, detail))?;
    let relative = path.strip_prefix(root).unwrap().as_os_str().as_bytes();
    digest.update((relative.len() as u64).to_le_bytes());
    digest.update(relative);
    digest.update(info.permissions().mode().to_le_bytes());
    if info.is_dir() {
        digest.update(b"D");
        let entries = fs::read_dir(path)
            .and_then(|entries| {
                entries
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<io::Result<Vec<_>>>()
            })
            .map_err(|detail| error(path, detail))?;
        let mut entries = entries;
        entries.sort();
        for entry in entries {
            hash_tree(root, &entry, digest)?;
        }
    } else if info.file_type().is_symlink() {
        digest.update(b"L");
        let value = fs::read_link(path).map_err(|detail| error(path, detail))?;
        let value = value.as_os_str().as_bytes();
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value);
    } else if info.is_file() {
        digest.update(b"F");
        digest.update(files::digest(path)?.ok_or_else(|| error(path, "bundle file disappeared"))?);
    } else {
        return Err(error(path, "bundle contains a special file"));
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), String> {
    let info = fs::symlink_metadata(path).map_err(|detail| error(path, detail))?;
    if info.file_type().is_symlink() {
        return Ok(());
    }
    if info.is_dir() {
        for entry in fs::read_dir(path).map_err(|detail| error(path, detail))? {
            sync_tree(&entry.map_err(|detail| error(path, detail))?.path())?;
        }
    }
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|detail| error(path, detail))?;
    Ok(())
}

fn error(path: &Path, detail: impl std::fmt::Display) -> String {
    format!(
        "refusal: journaled daemon bundle {}: {detail}",
        path.display()
    )
}

#[cfg(test)]
mod tests;
