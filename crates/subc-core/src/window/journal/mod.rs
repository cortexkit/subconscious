//! The placement journal is never opened by installer recovery.

pub mod content;
pub mod durable;
pub mod id;
pub(crate) mod owner;
pub(crate) mod paths;
mod records;
#[cfg(all(test, unix))]
mod tests;

pub use owner::{Liveness, Owner};
pub use records::{
    Action, Activation, Floor, Record, RestoredTarget, Role, SnapshotTransfer, Step,
};

use std::{
    fs::{self, File, TryLockError},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::setup::{
    install_journal::{open_journal, verify_journal_owner, VERSION},
    install_lock::open_lock_file,
};
use paths::{error_at, plain_dir};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Open,
    Interrupted,
    Reverting,
    RevertInterrupted,
    Unreadable,
    Kept,
    Abandoned,
    Reverted,
}

impl State {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Kept | Self::Abandoned | Self::Reverted)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Open => "open",
            Self::Interrupted => "interrupted",
            Self::Reverting => "reverting",
            Self::RevertInterrupted => "revert-interrupted",
            Self::Unreadable => "unreadable",
            Self::Kept => "kept",
            Self::Abandoned => "abandoned",
            Self::Reverted => "reverted",
        })
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub step: Step,
    pub done: bool,
    pub undone: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: String,
    pub note: Option<String>,
    pub began_ms: u64,
    pub steps: Vec<Entry>,
    pub record_count: usize,
    pub revert_owner: Option<Owner>,
    pub restored: Option<Vec<RestoredTarget>>,
    closed: Option<State>,
}

impl Snapshot {
    pub fn state(&self, liveness: impl Fn(&Owner) -> Liveness) -> State {
        if let Some(closed) = self.closed {
            return closed;
        }
        if let Some(owner) = &self.revert_owner {
            return if liveness(owner) == Liveness::Gone {
                State::RevertInterrupted
            } else {
                State::Reverting
            };
        }
        if self
            .unfinished()
            .is_some_and(|(_, owner)| liveness(owner) == Liveness::Gone)
        {
            return State::Interrupted;
        }
        State::Open
    }

    pub fn unfinished(&self) -> Option<(usize, &Owner)> {
        self.steps
            .iter()
            .position(|entry| !entry.done)
            .map(|i| (i + 1, &self.steps[i].step.owner))
    }

    pub fn steps_undone(&self) -> usize {
        self.steps.iter().filter(|entry| entry.undone).count()
    }

    fn add(&mut self, root: &Path, record: &Record) -> Result<(), String> {
        if self.closed.is_some() {
            return Err("record after window closed".into());
        }
        let reverting = self.revert_owner.is_some();
        match record {
            Record::Step { step } if !reverting && self.unfinished().is_none() => {
                step.validate()?;
                if !matches!(step.action, Action::Activation { .. }) {
                    paths::in_domain(root, &step.target)?;
                    if matches!(step.action, Action::Store { .. }) {
                        paths::in_domain(
                            root,
                            step.target.parent().ok_or("store target has no parent")?,
                        )?;
                    }
                }
                if step.action.placement().is_some() {
                    self.check_overlap(&step.target, step.action.kind())?;
                }
                self.steps.push(Entry {
                    step: step.as_ref().clone(),
                    done: false,
                    undone: false,
                });
            }
            Record::Done { index } if !reverting => {
                let entry = self.entry_mut(*index)?;
                if entry.done {
                    return Err("duplicate done mark".into());
                }
                entry.done = true;
            }
            Record::RevertStart { owner } if owner.valid() => {
                self.revert_owner = Some(owner.clone())
            }
            Record::Undone { index } if reverting => {
                let restored = self.restored.is_some();
                let entry = self.entry_mut(*index)?;
                if entry.undone
                    || restored != matches!(entry.step.action, Action::Activation { .. })
                {
                    return Err("undo mark in the wrong phase or already marked undone".into());
                }
                entry.undone = true;
            }
            Record::Restored { targets }
                if reverting
                    && self.restored.is_none()
                    && self.steps.iter().all(|entry| {
                        entry.undone || matches!(entry.step.action, Action::Activation { .. })
                    }) =>
            {
                records::validate_restored(self, targets)?;
                self.restored = Some(targets.clone());
            }
            Record::RevertEnd
                if reverting
                    && self.restored.is_some()
                    && self.steps.iter().all(|entry| entry.undone) =>
            {
                self.closed = Some(State::Reverted)
            }
            Record::Kept if !reverting && self.unfinished().is_none() => {
                self.closed = Some(State::Kept)
            }
            Record::Abandoned => self.closed = Some(State::Abandoned),
            _ => return Err("unexpected record for the window state".into()),
        }
        self.record_count += 1;
        Ok(())
    }

    fn entry_mut(&mut self, index: usize) -> Result<&mut Entry, String> {
        index
            .checked_sub(1)
            .and_then(|i| self.steps.get_mut(i))
            .ok_or_else(|| format!("unknown window step {index}"))
    }

    pub(crate) fn check_overlap(&self, target: &Path, kind: &str) -> Result<(), String> {
        for entry in &self.steps {
            let earlier = &entry.step;
            if earlier.action.placement().is_some()
                && paths::overlaps(&earlier.target, target)
                && (!paths::same_path(&earlier.target, target) || earlier.action.kind() != kind)
            {
                return Err(error_at(
                    target,
                    format!(
                        "overlaps earlier {} target {}",
                        earlier.action.kind(),
                        earlier.target.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn read(root: &Path, file: &mut File, id: &str) -> Result<(Snapshot, Option<u64>), String> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut lines = bytes[..end].split_inclusive(|byte| *byte == b'\n');
    let begin: Record = serde_json::from_slice(lines.next().ok_or("missing begin record")?)
        .map_err(|error| error.to_string())?;
    let (began_ms, note) = begin.validate_begin(id)?;
    let mut snapshot = Snapshot {
        id: id.into(),
        began_ms,
        note,
        steps: Vec::new(),
        record_count: 1,
        revert_owner: None,
        restored: None,
        closed: None,
    };
    for (i, line) in lines.enumerate() {
        let result = serde_json::from_slice(line)
            .map_err(|error| error.to_string())
            .and_then(|record| snapshot.add(root, &record));
        result.map_err(|error| format!("record {}: {error}", i + 2))?;
    }
    Ok((snapshot, (end != bytes.len()).then_some(end as u64)))
}

pub(crate) fn dir(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !id::valid(id) {
        return Err(format!(
            "invalid window id {id:?}; expected YYYYMMDD-HHMMSS-<4 hex>"
        ));
    }
    Ok(root.join("windows").join(id))
}

pub(crate) fn snapshot(root: &Path, id: &str) -> Result<Snapshot, String> {
    let dir = dir(root, id)?;
    if !plain_dir(&root.join("windows"))? || !plain_dir(&dir)? {
        return Err(format!("window {id} does not exist"));
    }
    let path = dir.join("journal");
    let decode = || -> Result<Snapshot, String> {
        let mut file = subc_os::open_regular_nofollow(&path).map_err(|error| error.to_string())?;
        verify_journal_owner(&file).map_err(|error| error.to_string())?;
        read(root, &mut file, id).map(|(snapshot, _)| snapshot)
    };
    decode().map_err(|error| error_at(&path, error))
}

pub(crate) fn lock_file(path: &Path) -> Result<WindowLock, String> {
    let lock = open_lock_file(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(WindowLock(lock)),
        Err(TryLockError::WouldBlock) => {
            Err(error_at(path, "another window command holds the lock"))
        }
        Err(TryLockError::Error(error)) => Err(error_at(path, error)),
    }
}

pub(crate) struct WindowLock(File);

impl Drop for WindowLock {
    fn drop(&mut self) {
        // On Unix, a concurrent fork can inherit the locked file until exec.
        // Unlock explicitly so the child's descriptor cannot extend the guard's hold.
        let _ = self.0.unlock();
    }
}

/// Keeps the per-window advisory lock until dropped.
pub struct Locked {
    _lock: WindowLock,
    root: PathBuf,
    dir: PathBuf,
    snapshot: Snapshot,
    failed_append: bool,
}

impl Locked {
    pub(crate) fn open(root: &Path, id: &str) -> Result<Self, String> {
        snapshot(root, id)?;
        let dir = dir(root, id)?;
        let lock = lock_file(&dir.join(".lock"))?;
        let snapshot = snapshot(root, id)?;
        Ok(Self {
            _lock: lock,
            dir,
            root: root.into(),
            snapshot,
            failed_append: false,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Return a warning, not a failed mutation, if only the human log fails.
    pub fn append(&mut self, record: Record) -> Result<Option<String>, String> {
        let path = self.dir.join("journal");
        if self.failed_append {
            return Err(error_at(
                &path,
                "previous append failed; reopen the window journal",
            ));
        }
        let append = || -> Result<Snapshot, String> {
            let mut file = open_journal(&path, false).map_err(|error| error.to_string())?;
            let info = file.metadata().map_err(|error| error.to_string())?;
            if !info.is_file() || crate::setup::install_journal::is_reparse_point(&info) {
                return Err("not a plain journal file".into());
            }
            verify_journal_owner(&file).map_err(|error| error.to_string())?;
            let (mut snapshot, torn) = read(&self.root, &mut file, &self.snapshot.id)?;
            if snapshot.record_count != self.snapshot.record_count {
                return Err("window journal changed while locked".into());
            }
            snapshot.add(&self.root, &record)?;
            if let Some(end) = torn {
                file.set_len(end).map_err(|error| error.to_string())?;
                durable::file(&path, &file)?;
            }
            write(&path, &mut file, &record)?;
            Ok(snapshot)
        };
        self.snapshot = append().map_err(|error| {
            self.failed_append = true;
            error_at(&path, error)
        })?;
        Ok(log(
            &self.dir,
            &self.snapshot.id,
            &record,
            Some(&self.snapshot),
        ))
    }
}

fn write(path: &Path, file: &mut File, record: &Record) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let previous = file
        .seek(SeekFrom::End(0))
        .map_err(|error| error_at(path, error))?;
    let persist = file
        .write_all(&bytes)
        .map_err(|error| error_at(path, error))
        .and_then(|()| durable::file(path, file));
    if let Err(error) = persist {
        // A failed write or sync must not leave a complete-looking record
        // that a later reader could mistake for an authorized operation.
        let reset = file.set_len(previous).and_then(|()| file.sync_all());
        return Err(match reset {
            Ok(()) => error,
            Err(reset) => format!("{error}; could not restore previous journal length: {reset}"),
        });
    }
    Ok(())
}

pub(crate) fn create(
    dir: &Path,
    id: &str,
    began_ms: u64,
    note: Option<String>,
) -> Result<Option<String>, String> {
    let begin = Record::Begin {
        version: VERSION,
        id: id.into(),
        began_ms,
        unprotected: true,
        note,
    };
    begin.validate_begin(id)?;
    let path = dir.join("journal");
    let mut file = open_journal(&path, true).map_err(|error| error_at(&path, error))?;
    write(&path, &mut file, &begin)?;
    durable::parent(&path)?;
    Ok(log(dir, id, &begin, None))
}

fn log(dir: &Path, id: &str, record: &Record, snapshot: Option<&Snapshot>) -> Option<String> {
    let (event, index) = match record {
        Record::Begin { .. } => ("begin", None),
        Record::Step { .. } => ("record", snapshot.map(|s| s.steps.len())),
        Record::Done { index } => ("done", Some(*index)),
        Record::RevertStart { .. } => ("revert-start", None),
        Record::Undone { index } => ("undo", Some(*index)),
        Record::Restored { .. } => ("restored", None),
        Record::RevertEnd => ("revert-end", None),
        Record::Kept => ("keep", None),
        Record::Abandoned => ("abandon", None),
    };
    let (position, kind, target) = index.zip(snapshot).map_or_else(
        || ("-".into(), "-", "\"-\"".into()),
        |(index, s)| {
            (
                format!("{index}/{}", s.steps.len()),
                s.steps[index - 1].step.action.kind(),
                super::json_string(&s.steps[index - 1].step.target.to_string_lossy()),
            )
        },
    );
    let timestamp = match record {
        Record::Begin { began_ms, .. } => *began_ms,
        _ => id::now_ms(),
    };
    let result = match record {
        Record::Step { step } => match step.action {
            Action::Store {
                snapshot_transfer, ..
            } => snapshot_transfer.message(),
            _ => "ok",
        },
        _ => "ok",
    };
    let result = super::json_string(result);
    let path = dir.join("log.txt");
    let append = || -> Result<(), String> {
        let mut file = match open_journal(&path, true) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                open_journal(&path, false).map_err(|error| error.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        let info = file.metadata().map_err(|error| error.to_string())?;
        if !info.is_file() || crate::setup::install_journal::is_reparse_point(&info) {
            return Err("not a plain log file".into());
        }
        verify_journal_owner(&file).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::End(0))
            .and_then(|_| {
                writeln!(
                    file,
                    "{} {id} {event} {position} {kind} {target} {result}",
                    id::log_time(timestamp)
                )
            })
            .map_err(|error| error.to_string())
    };
    append()
        .err()
        .map(|error| format!("warning: {}", error_at(&path, error)))
}

pub(crate) fn saved_indices(snapshot: &Snapshot) -> impl Iterator<Item = usize> + '_ {
    snapshot
        .steps
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| match &entry.step.action {
            Action::File {
                before: Some(_), ..
            }
            | Action::Bundle {
                before: Some(_), ..
            }
            | Action::Store { .. } => Some(i + 1),
            _ => None,
        })
}

pub(crate) fn entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| error_at(path, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error_at(path, error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}
