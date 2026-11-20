//! Journaled plain-file placement. Build or validate the candidate before
//! entering this code; the public path changes only after write-ahead and a
//! synced copy of the previous file.

use super::{is_reparse_point, sync_parent, Action, CleanupRefusal, FileStep, Journal, Step};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MARKER: &str = "transaction";

#[derive(Clone, Copy)]
pub(in crate::setup) enum Kind {
    File,
    Inventory,
}

pub(in crate::setup) enum Content<'a> {
    Bytes(&'a [u8], u32),
    File(&'a Path),
    Absent,
}

/// Prepare the write, then publish it atomically. Removing a file moves it to
/// the recovery directory so rollback needs no lossy copy.
pub(in crate::setup) fn place(
    journal: &mut Journal,
    target: &Path,
    kind: Kind,
    content: Content<'_>,
) -> Result<(), String> {
    let index = prepare(journal, target, kind, &content)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("file-write-ahead");
    act(journal, index, content)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("file-act");
    journal.done(index)
}

pub(super) fn prepare(
    journal: &mut Journal,
    target: &Path,
    kind: Kind,
    content: &Content<'_>,
) -> Result<usize, String> {
    let file = describe(journal, target, content)?;
    journal.prepare(Step {
        target: target.to_owned(),
        action: match kind {
            Kind::File => Action::File(file),
            Kind::Inventory => {
                let Content::Bytes(bytes, mode) = content else {
                    return Err(refusal(
                        target,
                        "final inventory step requires replayable bytes",
                    ));
                };
                let contents = std::str::from_utf8(bytes)
                    .map_err(|error| refusal(target, error))?
                    .to_owned();
                Action::Inventory {
                    file,
                    contents,
                    mode: *mode,
                }
            }
        },
    })
}

fn describe(journal: &Journal, target: &Path, content: &Content<'_>) -> Result<FileStep, String> {
    let before = digest(target)?;
    let after = match content {
        Content::Bytes(bytes, _) => Some(format!("{:x}", Sha256::digest(bytes))),
        Content::File(path) => {
            Some(digest(path)?.ok_or_else(|| refusal(path, "candidate is missing"))?)
        }
        Content::Absent => None,
    };
    let parent = target
        .parent()
        .ok_or_else(|| refusal(target, "file has no parent directory"))?;
    let staging_directory = parent.join(format!(
        ".ck-install-{}-{}",
        journal.id(),
        journal.next_index()
    ));
    Ok(FileStep {
        previous: before.as_ref().map(|_| staging_directory.join("previous")),
        staged: after.as_ref().map(|_| staging_directory.join("next")),
        staging_directory,
        before,
        after,
    })
}

pub(super) fn act(journal: &Journal, index: usize, content: Content<'_>) -> Result<(), String> {
    let step = journal.step(index);
    let file = step.action.file().expect("placement prepared a file step");
    perform(journal.id(), &step.target, file, content)
}

pub(in crate::setup) fn bootstrap(
    journal: &mut Journal,
    target: &Path,
    source: &Path,
    archive_sha256: String,
    version: String,
) -> Result<(), String> {
    let content = Content::File(source);
    let file = describe(journal, target, &content)?;
    let index = journal.prepare(Step {
        target: target.to_owned(),
        action: Action::Bootstrap {
            file,
            archive_sha256,
            version,
        },
    })?;
    #[cfg(feature = "test-support")]
    super::crash_hook("bootstrap-write-ahead");
    act(journal, index, content)?;
    #[cfg(feature = "test-support")]
    super::crash_hook("bootstrap-act");
    journal.done(index)
}

fn perform(id: &str, target: &Path, file: &FileStep, content: Content<'_>) -> Result<(), String> {
    let actual = digest(target)?;
    let gap = replacement_gap(id, target, file, &actual)?;
    if actual != file.before && !gap {
        return Err(refusal(target, "file changed after write-ahead"));
    }
    super::super::file_modes::ensure_private_parents(target.parent().expect("file parent"))?;
    if !owned_stage(id, &file.staging_directory)? {
        create_stage(id, &file.staging_directory)?;
    }
    if let Some(previous) = &file.previous {
        if file.after.is_some() && !moves_mapped_image(target) {
            // A hard link retains the exact previous inode and its metadata.
            if digest(previous)? != file.before {
                remove_partial(previous)?;
                fs::hard_link(target, previous)
                    .or_else(|_| copy(target, previous))
                    .map_err(|error| refusal(previous, error))?;
            }
            verify_digest(previous, &file.before)?;
            sync_parent(previous)?;
        }
    }
    if let Some(staged) = &file.staged {
        if digest(staged)? != file.after {
            remove_partial(staged)?;
            match content {
                Content::Bytes(bytes, mode) => create_bytes(staged, bytes, mode)?,
                Content::File(source) => {
                    copy(source, staged).map_err(|error| refusal(staged, error))?
                }
                Content::Absent => return Err(refusal(target, "missing journal candidate")),
            }
        }
        verify_digest(staged, &file.after)?;
        if moves_mapped_image(target) && file.before.is_some() {
            let previous = file
                .previous
                .as_deref()
                .ok_or_else(|| refusal(target, "missing previous image location"))?;
            if !gap {
                verify_digest(target, &file.before)?;
                move_out(target, previous)?;
            }
            #[cfg(feature = "test-support")]
            super::crash_hook("windows-replacement-gap");
            if let Err(error) = move_out(staged, target) {
                if digest(target)?.is_none() {
                    move_out(previous, target).map_err(|restore| {
                        format!("{error}; could not restore the previous image: {restore}")
                    })?;
                }
                return Err(error);
            }
            return Ok(());
        }
        verify_digest(target, &file.before)?;
        replace(staged, target)?;
    } else if let Some(previous) = &file.previous {
        verify_digest(target, &file.before)?;
        move_out(target, previous)?;
    }
    Ok(())
}

fn moves_mapped_image(target: &Path) -> bool {
    cfg!(windows)
        && target
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

fn replacement_gap(
    id: &str,
    target: &Path,
    file: &FileStep,
    actual: &Option<String>,
) -> Result<bool, String> {
    if !moves_mapped_image(target)
        || actual.is_some()
        || file.before.is_none()
        || file.after.is_none()
    {
        return Ok(false);
    }
    let Some(previous) = &file.previous else {
        return Ok(false);
    };
    Ok(owned_stage(id, &file.staging_directory)? && digest(previous)? == file.before)
}

pub(super) fn is_replacement_gap(id: &str, step: &Step) -> Result<bool, String> {
    let file = step.action.file().expect("checked a file step");
    replacement_gap(id, &step.target, file, &digest(&step.target)?)
}

fn move_out(source: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    return subc_os::durable_move_to_new_name(source, target)
        .map_err(|error| refusal(source, error));
    #[cfg(not(windows))]
    replace(source, target)
}

fn remove_partial(path: &Path) -> Result<(), String> {
    if metadata(path)?.is_some() {
        fs::remove_file(path).map_err(|error| refusal(path, error))?;
    }
    Ok(())
}

pub(in crate::setup) fn verify(step: &Step) -> Result<bool, String> {
    let file = step.action.file().expect("verified a file step");
    if digest(&step.target)? != file.after {
        return Ok(false);
    }
    if moves_mapped_image(&step.target) && file.after.is_some() {
        if let Some(previous) = &file.previous {
            return Ok(digest(previous)? == file.before);
        }
    }
    Ok(true)
}

pub(in crate::setup) fn can_replay(id: &str, step: &Step) -> Result<bool, String> {
    let file = step.action.file().expect("checked replay of a file step");
    let actual = digest(&step.target)?;
    if actual != file.before && !replacement_gap(id, &step.target, file, &actual)? {
        return Ok(false);
    }
    if let Action::Inventory { contents, .. } = &step.action {
        return Ok(file.after.as_deref() == Some(&format!("{:x}", Sha256::digest(contents))));
    }
    if let Some(staged) = &file.staged {
        return Ok(owned_stage(id, &file.staging_directory)? && digest(staged)? == file.after);
    }
    Ok(true)
}

pub(in crate::setup) fn replay(id: &str, step: &Step) -> Result<(), String> {
    let file = step.action.file().expect("replayed a file step");
    let content = match &step.action {
        Action::Inventory { contents, mode, .. } => Content::Bytes(contents.as_bytes(), *mode),
        _ => Content::Absent,
    };
    perform(id, &step.target, file, content)
}

pub(in crate::setup) fn restore(id: &str, step: &Step) -> Result<(), String> {
    let file = step.action.file().expect("restored a file step");
    let target = &step.target;
    let actual = digest(target)?;
    if actual == file.before {
        return Ok(());
    }
    if actual != file.after && !replacement_gap(id, target, file, &actual)? {
        let before = file.before.as_deref().unwrap_or("absent");
        let after = file.after.as_deref().unwrap_or("absent");
        let current = actual.as_deref().unwrap_or("absent");
        let backup = file.previous.as_ref().map_or_else(String::new, |path| {
            format!("; the previous bytes may be retained at {}", path.display())
        });
        return Err(refusal(
            target,
            format!("unexpected file state (SHA-256 or absent): current {current}, recorded before {before}, recorded after {after}{backup}. Save your edited file elsewhere, restore either recorded state, then rerun the installer command"),
        ));
    }
    if !owned_stage(id, &file.staging_directory)? {
        return Err(refusal(
            &file.staging_directory,
            "journal recovery data are missing",
        ));
    }
    if let Some(previous) = &file.previous {
        verify_digest(previous, &file.before)?;
        if actual.is_some() && moves_mapped_image(target) {
            let removed = file.staging_directory.join("undone");
            move_out(target, &removed)?;
            if let Err(error) = move_out(previous, target) {
                if digest(target)?.is_none() {
                    move_out(&removed, target).map_err(|restore| {
                        format!("{error}; could not restore the replacement image: {restore}")
                    })?;
                }
                return Err(error);
            }
        } else if actual.is_none() {
            move_out(previous, target)?;
        } else {
            replace(previous, target)?;
        }
    } else {
        // Take the published bytes off the public path with a durable rename.
        // The staging directory already proves where they may go.
        let removed = file.staging_directory.join("undone");
        if metadata(&removed)?.is_some() {
            return Err(refusal(&removed, "undo destination already exists"));
        }
        move_out(target, &removed)?;
    }
    #[cfg(feature = "test-support")]
    super::crash_hook("file-undo");
    Ok(())
}

pub(in crate::setup) fn prepare_discard(id: &str, step: &Step) -> Result<Option<String>, String> {
    let Some(file) = step.action.file() else {
        return Ok(None);
    };
    let dir = &file.staging_directory;
    match owned_stage(id, dir) {
        Ok(false) => Ok(None),
        Ok(true) => directory_identity(dir).map(Some),
        Err(error) => {
            if interrupted_empty_stage(id, dir)? {
                directory_identity(dir).map(Some)
            } else {
                Err(error)
            }
        }
    }
}

pub(in crate::setup) fn discard(
    id: &str,
    step: &Step,
    dir: &Path,
    identity: &str,
) -> Result<Vec<PathBuf>, CleanupRefusal> {
    discard_with(id, step, dir, identity, || ())
}

pub(super) fn discard_with(
    id: &str,
    step: &Step,
    dir: &Path,
    identity: &str,
    after_marker: impl FnOnce(),
) -> Result<Vec<PathBuf>, CleanupRefusal> {
    if metadata(dir)
        .map_err(|error| CleanupRefusal::at(dir, error))?
        .is_none()
    {
        return Ok(Vec::new());
    }
    if directory_identity(dir).map_err(|error| CleanupRefusal::at(dir, error))? != identity {
        return Err(CleanupRefusal::at(
            dir,
            "staging directory changed identity after cleanup was authorized",
        ));
    }
    let marker = dir.join(MARKER);
    if metadata(&marker)
        .map_err(|error| CleanupRefusal::at(&marker, error))?
        .is_some()
    {
        if let Err(error) = owned_stage(id, dir) {
            if !interrupted_empty_stage(id, dir).map_err(|error| CleanupRefusal::at(dir, error))? {
                return Err(CleanupRefusal::at(&marker, error));
            }
        }
        let mut busy = Vec::new();
        for entry in fs::read_dir(dir).map_err(|error| CleanupRefusal::at(dir, error))? {
            let entry = entry.map_err(|error| CleanupRefusal::at(dir, error))?;
            if entry.file_name() == MARKER {
                continue;
            }
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| CleanupRefusal::at(&path, error))?;
            match entry.file_name().to_str() {
                Some("previous" | "next")
                    if matches!(step.action, Action::Symlink(_)) && kind.is_symlink() =>
                {
                    fs::remove_file(&path).map_err(|error| CleanupRefusal::at(&path, error))?;
                }
                Some("previous" | "next" | "undone")
                    if kind.is_file()
                        && metadata(&path)
                            .map_err(|error| CleanupRefusal::at(&path, error))?
                            .is_some_and(|info| !is_reparse_point(&info)) =>
                {
                    if let Err(error) = fs::remove_file(&path) {
                        if cleanup_is_busy(&path, &error) {
                            busy.push(path);
                        } else {
                            return Err(CleanupRefusal::at(&path, error));
                        }
                    }
                }
                Some("previous" | "next" | "undone")
                    if matches!(step.action, Action::Bundle(_)) && kind.is_dir() =>
                {
                    fs::remove_dir_all(&path).map_err(|error| CleanupRefusal::at(&path, error))?;
                }
                _ => return Err(CleanupRefusal::at(&path, "unknown staged file")),
            }
        }
        if !busy.is_empty() {
            return Ok(busy);
        }
        fs::remove_file(&marker).map_err(|error| CleanupRefusal::at(&marker, error))?;
        sync_parent(&marker).map_err(|error| CleanupRefusal::at(&marker, error))?;
        after_marker();
    }
    // rmdir never deletes contents, even if an unrelated file appears after
    // the checks or the marker was deleted by an interrupted cleanup.
    fs::remove_dir(dir).map_err(|error| CleanupRefusal::at(dir, error))?;
    sync_parent(dir).map_err(|error| CleanupRefusal::at(dir, error))?;
    Ok(Vec::new())
}

#[cfg(windows)]
fn cleanup_is_busy(path: &Path, error: &io::Error) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    let in_use = |error: &io::Error| matches!(error.raw_os_error(), Some(32 | 33 | 1224));
    if in_use(error) {
        return true;
    }
    // Deleting a mapped image may report Access Denied rather than a sharing
    // violation. Probe the already checked regular file without writing it.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    error.raw_os_error() == Some(5)
        && OpenOptions::new()
            .write(true)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(path)
            .is_err_and(|error| in_use(&error))
}

#[cfg(not(windows))]
fn cleanup_is_busy(_path: &Path, _error: &io::Error) -> bool {
    false
}

fn directory_identity(dir: &Path) -> Result<String, String> {
    let descriptor = open_staging_directory(dir).map_err(|error| refusal(dir, error))?;
    let info = descriptor.metadata().map_err(|error| refusal(dir, error))?;
    if !info.is_dir() || is_reparse_point(&info) {
        return Err(refusal(dir, "staging path is not a plain directory"));
    }
    let identity =
        subc_os::retained_file_identity(&descriptor).map_err(|error| refusal(dir, error))?;
    #[cfg(unix)]
    return Ok(format!("unix:{}:{}", identity.device, identity.inode));
    #[cfg(windows)]
    return Ok(format!(
        "windows:{}:{:02x?}",
        identity.volume_serial_number, identity.file_id
    ));
}

#[cfg(unix)]
fn open_staging_directory(path: &Path) -> io::Result<fs::File> {
    use rustix::fs::{open, Mode, OFlags};
    Ok(fs::File::from(open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

#[cfg(windows)]
fn open_staging_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const READ_ATTRIBUTES: u32 = 0x80;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .access_mode(READ_ATTRIBUTES)
        .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
        .open(path)
}

pub(super) fn create_stage(id: &str, dir: &Path) -> Result<(), String> {
    subc_os::create_private_directory_exclusive(dir).map_err(|error| refusal(dir, error))?;
    let marker = dir.join(MARKER);
    create_bytes(&marker, id.as_bytes(), 0o600)?;
    sync_parent(&marker)?;
    sync_parent(dir)
}

pub(super) fn owned_stage(id: &str, dir: &Path) -> Result<bool, String> {
    let Some(info) = metadata(dir)? else {
        return Ok(false);
    };
    let marker = dir.join(MARKER);
    if !info.is_dir()
        || is_reparse_point(&info)
        || !metadata(&marker)?.is_some_and(|marker| marker.is_file() && !is_reparse_point(&marker))
    {
        return Err(refusal(
            dir,
            "staging directory has no valid transaction marker",
        ));
    }
    let mut recorded = Vec::new();
    subc_os::open_regular_nofollow(&marker)
        .map_err(|error| refusal(&marker, error))?
        .take(id.len() as u64 + 1)
        .read_to_end(&mut recorded)
        .map_err(|error| refusal(&marker, error))?;
    if recorded != id.as_bytes() {
        return Err(refusal(
            &marker,
            "staging directory belongs to another transaction",
        ));
    }
    Ok(true)
}

/// An interrupted mkdir/marker write cannot have made any backups. Only
/// authorize cleanup when there is nothing except the partial marker.
fn interrupted_empty_stage(id: &str, dir: &Path) -> Result<bool, String> {
    if !metadata(dir)?.is_some_and(|info| info.is_dir() && !is_reparse_point(&info)) {
        return Ok(false);
    }
    let mut entries = fs::read_dir(dir).map_err(|error| refusal(dir, error))?;
    let Some(first) = entries
        .next()
        .transpose()
        .map_err(|error| refusal(dir, error))?
    else {
        return Ok(true);
    };
    if first.file_name() != MARKER || entries.next().is_some() {
        return Ok(false);
    }
    let marker = first.path();
    let mut bytes = Vec::new();
    subc_os::open_regular_nofollow(&marker)
        .map_err(|error| refusal(&marker, error))?
        .take(id.len() as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| refusal(&marker, error))?;
    Ok(bytes.len() < id.len() && id.as_bytes().starts_with(&bytes))
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(refusal(path, error)),
    }
}

pub(in crate::setup) fn digest(path: &Path) -> Result<Option<String>, String> {
    let mut fd = match subc_os::open_regular_nofollow(path) {
        Ok(fd) => fd,
        Err(subc_os::OpenRegularError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(refusal(path, error)),
    };
    let mut hash = Sha256::new();
    io::copy(&mut fd, &mut hash).map_err(|error| refusal(path, error))?;
    Ok(Some(format!("{:x}", hash.finalize())))
}

fn verify_digest(path: &Path, expected: &Option<String>) -> Result<(), String> {
    if digest(path)? != *expected {
        return Err(refusal(
            path,
            "file does not have the digest recorded in the journal",
        ));
    }
    Ok(())
}

fn create_bytes(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path).map_err(|error| refusal(path, error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| refusal(path, error))
}

fn copy(source: &Path, target: &Path) -> io::Result<()> {
    let mut source = subc_os::open_regular_nofollow(source).map_err(io::Error::other)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut source, &mut target)?;
    // Preserve Unix execute bits. On Windows the staged file must remain
    // writable so the rename can open it and flush the publication to disk.
    #[cfg(unix)]
    target.set_permissions(source.metadata()?.permissions())?;
    target.sync_all()
}

fn replace(source: &Path, target: &Path) -> Result<(), String> {
    subc_os::durable_replace(source, target).map_err(|error| refusal(target, error))?;
    sync_parent(source)
}

fn refusal(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("refusal: install journal file {}: {detail}", path.display())
}
