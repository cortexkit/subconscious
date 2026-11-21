//! Retry deleting backups still in use after an installation committed or
//! rolled back. Retired journals never replay or undo installation steps.

use super::{
    bootstrap::FileRecovery, is_reparse_point, journal_error, sync_parent, valid_id, Journal,
    Outcome, NAME,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(in crate::setup) fn path(manifest: &Path) -> PathBuf {
    manifest.with_file_name("installer.retired")
}

pub(super) fn leave(journal: Journal, busy: &[PathBuf]) -> Result<(), String> {
    if journal.state.outcome.is_none() {
        return Err(journal_error(
            &journal.path,
            "only a closed journal can be retired",
        ));
    }
    if journal.path.file_name().is_some_and(|name| name == NAME) {
        let dir = path(&journal.path);
        if !check_directory(&dir)? {
            subc_os::create_private_directory_exclusive(&dir)
                .map_err(|error| retired_refusal(&dir, error))?;
            sync_parent(&dir).map_err(|error| inspection_hint(&dir, &error))?;
        }
        let destination = dir.join(format!("{}.journal", journal.id()));
        let source = journal.path;
        drop(journal.file);
        move_closed(&source, &destination)
            .map_err(|error| inspection_hint(&destination, &error))?;
    }
    for file in busy {
        report_busy(file);
    }
    Ok(())
}

pub(in crate::setup) fn report_busy(file: &Path) {
    crate::setup::messages::warning(format!("{} is still in use and will be removed automatically once the program exits on the next applying installer run", file.display()));
}

#[cfg(windows)]
fn move_closed(source: &Path, target: &Path) -> Result<(), String> {
    subc_os::durable_move_to_new_name(source, target).map_err(|error| journal_error(target, error))
}

#[cfg(not(windows))]
fn move_closed(source: &Path, target: &Path) -> Result<(), String> {
    // All installer commands hold the same lock while assigning a retired name.
    if fs::symlink_metadata(target).is_ok() {
        return Err(journal_error(target, "retired journal already exists"));
    }
    fs::rename(source, target).map_err(|error| journal_error(target, error))?;
    sync_parent(target)?;
    sync_parent(source)
}

pub(in crate::setup) fn sweep(manifest: &Path) -> Result<(), String> {
    for file in entries(manifest)? {
        let (journal, outcome) = open(file.clone())?;
        journal
            .discard(&mut FileRecovery, outcome)
            .map_err(|error| inspection_hint(&file, &String::from(error)))?;
    }
    Ok(())
}

pub(super) fn entries(manifest: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = path(manifest);
    if !check_directory(&dir)? {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| retired_refusal(&dir, error))? {
        let entry = entry.map_err(|error| retired_refusal(&dir, error))?;
        if entry.path().extension().is_some_and(|ext| ext == "journal") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn open(file: PathBuf) -> Result<(Journal, Outcome), String> {
    let id = file
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| valid_id(name))
        .ok_or_else(|| retired_refusal(&file, "invalid retired journal name"))?
        .to_owned();
    let journal =
        Journal::open_path(file.clone()).map_err(|error| inspection_hint(&file, &error))?;
    if journal.state.id.as_deref() != Some(id.as_str()) {
        return Err(retired_refusal(
            &journal.path,
            "retired journal id does not match its name",
        ));
    }
    let outcome = journal.state.outcome.ok_or_else(|| {
        retired_refusal(
            &journal.path,
            "retired journal is not closed; cannot sweep it",
        )
    })?;
    Ok((journal, outcome))
}

fn check_directory(dir: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(retired_refusal(dir, error)),
    };
    if !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err(retired_refusal(
            dir,
            "retired journal directory must be a plain directory",
        ));
    }
    #[cfg(windows)]
    subc_os::windows_acl::verify_private_dir(dir).map_err(|error| retired_refusal(dir, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(retired_refusal(
                dir,
                "retired journal directory must be owner-only",
            ));
        }
    }
    Ok(true)
}

fn retired_refusal(path: &Path, error: impl std::fmt::Display) -> String {
    inspection_hint(path, &journal_error(path, error))
}

fn inspection_hint(path: &Path, error: &str) -> String {
    let where_to_move = if path
        .file_name()
        .is_some_and(|name| name == "installer.retired")
    {
        "aside"
    } else {
        "out of installer.retired"
    };
    format!(
        "{error}; inspect {} and move it {where_to_move}, then rerun",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_refusals_name_the_journal_to_inspect_and_move() {
        for case in ["bad-name", "wrong-id", "open"] {
            let fixture = cortexkit_test_support::ScratchDir::new("retired-journal-refusal");
            let manifest = fixture.join("manifest");
            let journal = Journal::create(&manifest).unwrap();
            let id = journal.id().to_owned();
            drop(journal);
            let dirname = path(&manifest);
            subc_os::create_private_directory_exclusive(&dirname).unwrap();
            let file = dirname.join(match case {
                "bad-name" => "wrong-name.journal".into(),
                "wrong-id" => "0-0.journal".into(),
                _ => format!("{id}.journal"),
            });
            fs::rename(Journal::path(&manifest), &file).unwrap();
            let before = fs::read(&file).unwrap();
            let error = sweep(&manifest).unwrap_err();
            assert!(
                error.contains(&format!(
                    "inspect {} and move it out of installer.retired, then rerun",
                    file.display()
                )),
                "{case}: {error}"
            );
            assert_eq!(
                fs::read(file).unwrap(),
                before,
                "a retired journal must never be undone"
            );
        }
    }

    #[test]
    fn retired_refusal_explains_how_to_move_an_invalid_directory_aside() {
        let fixture = cortexkit_test_support::ScratchDir::new("retired-directory-refusal");
        let manifest = fixture.join("manifest");
        let dir = path(&manifest);
        fs::write(&dir, b"unexpected file").unwrap();
        let error = sweep(&manifest).unwrap_err();
        assert!(
            error.contains(&format!(
                "inspect {} and move it aside, then rerun",
                dir.display()
            )),
            "{error}"
        );
        assert_eq!(fs::read(dir).unwrap(), b"unexpected file");
    }
}
