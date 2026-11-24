mod host;
mod placement;
mod report;

#[cfg(test)]
mod tests;

pub use placement::{RecordRequest, RECORD_HELP};
pub use report::{Close, CurrentStep, List, StepSummary, Summary, WindowResult};

use std::{ffi::OsStr, io, path::Path};

use super::{
    journal::{self, durable, id, paths, Action, Liveness, Locked, Owner, Record, Snapshot, State},
    WindowError, Windows,
};
use crate::{
    setup::install_journal::Journal,
    stack::{self, WindowCheck},
};
use host::{Host, SystemHost};

const PROTECTED: &str = "protected installs are not covered by restart windows yet";
const UNCONFIRMED: &str =
    "the daemon is not answering, so the install can't be confirmed unprotected";
const MANUAL: &str = "recover this window by hand";

impl Windows {
    pub fn begin(&self, note: Option<&str>) -> Result<String, WindowError> {
        self.begin_with(
            note,
            &mut SystemHost(self),
            id::now_ms(),
            id::suffix,
            || Ok(()),
        )
    }

    fn begin_with(
        &self,
        note: Option<&str>,
        host: &mut impl Host,
        now: u64,
        mut suffix: impl FnMut() -> u16,
        before_publish: impl FnOnce() -> Result<(), String>,
    ) -> Result<String, WindowError> {
        id::validate_note(note)?;
        self.installer_absent()?;
        refuse_begin(&self.ensure_readable()?, host)?;
        check_stamped(host, true)?;
        let windows = self.root.join("windows");
        durable::ensure_dir(&self.root, &windows)?;
        let _lock = journal::lock_file(&windows.join(".lock"))?;
        self.installer_absent()?;
        refuse_begin(&self.ensure_readable()?, host)?;
        for path in journal::entries(&windows)? {
            if path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(".tmp-"))
                && paths::info(&path)?.is_some_and(|info| info.is_dir())
            {
                durable::remove_artifact(&path)?;
            }
        }
        let (id, temporary, destination) = (0..=u16::MAX)
            .find_map(|_| {
                let id = id::candidate(now, suffix());
                let temporary = windows.join(format!(".tmp-{id}"));
                let destination = windows.join(&id);
                let create = || -> Result<bool, String> {
                    if paths::info(&destination)?.is_some() {
                        return Ok(false);
                    }
                    match durable::create_dir_exclusive(&temporary) {
                        Ok(()) => Ok(true),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                        Err(error) => Err(paths::error_at(&temporary, error)),
                    }
                };
                create()
                    .map(|created| created.then_some((id, temporary, destination)))
                    .transpose()
            })
            .ok_or("could not allocate a window id")??;
        durable::parent(&temporary).map_err(WindowError::failed)?;
        let warning = journal::create(&temporary, &id, now, note.map(str::to_owned))
            .map_err(WindowError::failed)?;
        before_publish().map_err(WindowError::failed)?;
        durable::rename_new(&temporary, &destination).map_err(WindowError::failed)?;
        emit_warning(warning);
        Ok(id)
    }

    pub fn current(&self) -> Result<Option<String>, WindowError> {
        self.current_with(&mut SystemHost(self))
    }

    fn current_with(&self, host: &mut impl Host) -> Result<Option<String>, WindowError> {
        let rows = self.ensure_readable()?;
        let mut active = None;
        for snapshot in rows.into_iter().flatten() {
            let state = snapshot.state(|owner| host.liveness(owner));
            match state {
                State::Open if active.is_none() => active = Some(snapshot.id),
                State::Open => return Err("more than one window is open".into()),
                _ if state.terminal() => (),
                _ => return Err(state_error(&snapshot, state, host)),
            }
        }
        if active.is_none() {
            check_stamped(host, false)?;
        }
        Ok(active)
    }

    pub fn list(&self) -> Result<List, WindowError> {
        self.list_with(&mut SystemHost(self))
    }

    fn list_with(&self, host: &mut impl Host) -> Result<List, WindowError> {
        let rows = self.scan()?;
        if rows.is_empty() {
            check_stamped(host, false)?;
        }
        let windows = rows
            .into_iter()
            .take(20)
            .map(|row| Summary::new(&self.root, row, |owner| host.liveness(owner)))
            .collect();
        Ok(List::new(windows))
    }

    pub fn record(
        &self,
        id: &str,
        owner_pid: u32,
        request: RecordRequest,
    ) -> Result<usize, WindowError> {
        self.record_with(id, owner_pid, request, &mut SystemHost(self))
    }

    fn record_with(
        &self,
        id: &str,
        owner_pid: u32,
        request: RecordRequest,
        host: &mut impl Host,
    ) -> Result<usize, WindowError> {
        let mut locked = self.open_command(id, host)?;
        let snapshot = locked.snapshot();
        require_idle(snapshot, host)?;
        let owner = host.owner(owner_pid)?;
        check_stamped(host, false)?;
        let index = snapshot.steps.len() + 1;
        let prepare = placement::prepare(&self.root, snapshot, request)?;
        let step =
            placement::materialize(prepare, locked.directory(), snapshot, index, owner, host)?;
        let transfer = match step.action {
            Action::Store {
                snapshot_transfer, ..
            } => Some(snapshot_transfer),
            _ => None,
        };
        emit_warning(
            locked
                .append(Record::Step {
                    step: Box::new(step),
                })
                .map_err(WindowError::failed)?,
        );
        if let Some(transfer) = transfer {
            eprintln!("window {id} step {index}: {}", transfer.message());
        }
        Ok(index)
    }

    pub fn done(&self, id: &str, owner_pid: u32, index: usize) -> Result<(), WindowError> {
        self.done_with(id, owner_pid, index, &mut SystemHost(self))
    }

    fn done_with(
        &self,
        id: &str,
        owner_pid: u32,
        index: usize,
        host: &mut impl Host,
    ) -> Result<(), WindowError> {
        let mut locked = self.open_command(id, host)?;
        let snapshot = locked.snapshot();
        let state = snapshot.state(|owner| host.liveness(owner));
        if state != State::Open {
            return Err(state_error(snapshot, state, host));
        }
        let entry = index
            .checked_sub(1)
            .and_then(|index| snapshot.steps.get(index))
            .ok_or_else(|| format!("window {id} has no step {index}"))?;
        let owner = host.owner(owner_pid)?;
        if !entry.step.owner.same_process(&owner) {
            return Err(format!(
                "window {id} step {index} belongs to {}",
                entry.step.owner.describe()
            )
            .into());
        }
        if entry.done {
            return Ok(());
        }
        check_stamped(host, false)?;
        placement::verify_done(&self.root, &entry.step, host)?;
        emit_warning(
            locked
                .append(Record::Done { index })
                .map_err(WindowError::failed)?,
        );
        Ok(())
    }

    pub fn keep(&self, id: Option<&str>) -> Result<Close, WindowError> {
        self.close(id, Closing::Keep, &mut SystemHost(self), stack::confirm)
    }

    pub fn abandon(&self, id: &str) -> Result<Close, WindowError> {
        self.close(
            Some(id),
            Closing::Abandon,
            &mut SystemHost(self),
            stack::confirm,
        )
    }

    fn close(
        &self,
        id: Option<&str>,
        closing: Closing,
        host: &mut impl Host,
        confirm: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<Close, WindowError> {
        if let Some(id) = id {
            journal::dir(&self.root, id)?;
        }
        let rows = self.ensure_readable()?;
        check_stamped(host, false)?;
        let mut rows = rows.into_iter().flatten();
        let plan = if let Some(id) = id {
            rows.find(|snapshot| snapshot.id == id)
                .ok_or_else(|| format!("window {id} does not exist"))?
        } else {
            let snapshots: Vec<_> = rows.collect();
            snapshots
                .iter()
                .find(|snapshot| snapshot.state(|owner| host.liveness(owner)) == State::Open)
                .or_else(|| {
                    snapshots.iter().find(|snapshot| {
                        snapshot.state(|owner| host.liveness(owner)) == State::Kept
                            && self
                                .root
                                .join("windows")
                                .join(&snapshot.id)
                                .join("saved")
                                .symlink_metadata()
                                .is_ok()
                    })
                })
                .or_else(|| {
                    snapshots
                        .iter()
                        .find(|snapshot| !snapshot.state(|owner| host.liveness(owner)).terminal())
                })
                .cloned()
                .ok_or("no open window or kept window with pending cleanup")?
        };
        let dir = journal::dir(&self.root, &plan.id)?;
        let state = allow_close(&plan, &dir, closing, host)?;
        confirm(&closing.prompt(&plan))?;
        let mut locked = self.lock(&plan.id, plan.record_count)?;
        let current = allow_close(locked.snapshot(), locked.directory(), closing, host)?;
        if state != current {
            return Err(format!("window {} changed while confirming the plan", plan.id).into());
        }
        check_stamped(host, false)?;
        let mut result = Close {
            id: plan.id,
            outstanding: Vec::new(),
            warnings: Vec::new(),
        };
        if closing == Closing::Abandon {
            result.outstanding.extend(
                locked
                    .snapshot()
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| !entry.undone)
                    .map(|(index, entry)| StepSummary::new(index, entry)),
            );
        }
        if state != State::Kept {
            let record = if closing == Closing::Keep {
                Record::Kept
            } else {
                Record::Abandoned
            };
            result
                .warnings
                .extend(locked.append(record).map_err(WindowError::failed)?);
        }
        if closing == Closing::Keep {
            let saved = locked.directory().join("saved");
            durable::remove_artifact(&saved).map_err(|error| {
                WindowError::failed(format!(
                    "window {} kept, cleanup pending: {error}",
                    result.id
                ))
            })?;
        }
        Ok(result)
    }

    fn installer_absent(&self) -> Result<(), WindowError> {
        let manifest = self.root.join("installer-manifest.json");
        if Journal::exists(&manifest)? {
            return Err(format!(
                "installer journal {} exists",
                Journal::path(&manifest).display()
            )
            .into());
        }
        Ok(())
    }

    fn open_command(&self, id: &str, host: &mut impl Host) -> Result<Locked, WindowError> {
        let path = journal::dir(&self.root, id)?;
        self.ensure_readable()?;
        if !paths::plain_dir(&path)? {
            check_stamped(host, false)?;
        }
        Locked::open(&self.root, id).map_err(Into::into)
    }

    pub(super) fn ensure_readable(&self) -> Result<Vec<Row>, WindowError> {
        let rows = self.scan()?;
        if let Some(Err((id, error))) = rows.iter().find(|row| row.is_err()) {
            return Err(format!("window {id} is unreadable: {error}; {MANUAL}").into());
        }
        Ok(rows)
    }

    fn scan(&self) -> Result<Vec<Row>, WindowError> {
        let windows = self.root.join("windows");
        if !paths::plain_dir(&windows)? {
            return Ok(Vec::new());
        }
        let mut rows: Vec<_> = journal::entries(&windows)?
            .into_iter()
            .filter_map(|path| {
                let id = path.file_name()?.to_str()?;
                if !id::valid(id) {
                    return None;
                }
                Some(journal::snapshot(&self.root, id).map_err(|error| (id.to_string(), error)))
            })
            .collect();
        rows.sort_by_cached_key(|row| std::cmp::Reverse(row_key(row)));
        Ok(rows)
    }

    pub(super) fn check_stack(&self, except: Option<&str>) -> WindowCheck {
        match self.scan() {
            Ok(rows) => {
                let mut open = false;
                for row in rows {
                    match row {
                        Err((id, error)) => return WindowCheck::Unreadable { id, error },
                        Ok(snapshot) if Some(snapshot.id.as_str()) == except => continue,
                        Ok(snapshot) => match snapshot.state(Owner::liveness) {
                            State::Reverting => return WindowCheck::Reverting { id: snapshot.id },
                            State::RevertInterrupted => {
                                return WindowCheck::RevertInterrupted { id: snapshot.id }
                            }
                            State::Open | State::Interrupted => open = true,
                            _ => (),
                        },
                    }
                }
                if open {
                    WindowCheck::Open
                } else {
                    WindowCheck::NoWindow
                }
            }
            Err(error) => WindowCheck::Unreadable {
                id: "windows".into(),
                error: error.message,
            },
        }
    }

    pub(super) fn check_protection(&self, required: bool) -> Result<(), WindowError> {
        check_stamped(&mut SystemHost(self), required)
    }
}

pub(super) type Row = Result<Snapshot, (String, String)>;

fn row_key(row: &Row) -> (String, u64, String) {
    match row {
        Ok(snapshot) => (
            snapshot.id[..15].into(),
            snapshot.began_ms % 1000,
            snapshot.id.clone(),
        ),
        Err((id, _)) => (id[..15].into(), 0, id.clone()),
    }
}

fn check_stamped(host: &mut impl Host, required: bool) -> Result<(), WindowError> {
    match host.stamped()? {
        Some(true) => Err(PROTECTED.into()),
        None if required => Err(UNCONFIRMED.into()),
        _ => Ok(()),
    }
}

fn refuse_begin(rows: &[Row], host: &impl Host) -> Result<(), WindowError> {
    for snapshot in rows.iter().filter_map(|row| row.as_ref().ok()) {
        let state = snapshot.state(|owner| host.liveness(owner));
        if !state.terminal() {
            return Err(state_error(snapshot, state, host));
        }
    }
    Ok(())
}

fn state_error(snapshot: &Snapshot, state: State, host: &impl Host) -> WindowError {
    let detail = match state {
        State::Interrupted => "; run `ck window revert` or `ck window abandon`".into(),
        State::Reverting => snapshot
            .revert_owner
            .as_ref()
            .map_or_else(String::new, |owner| {
                format!("; revert owner {}", describe_owner(owner, host))
            }),
        _ => String::new(),
    };
    format!("window {} is {state}{detail}", snapshot.id).into()
}

fn describe_owner(owner: &Owner, host: &impl Host) -> String {
    let description = owner.describe();
    if host.liveness(owner) == Liveness::Unreadable {
        format!("{description} (identity unreadable)")
    } else {
        description
    }
}

fn require_idle(snapshot: &Snapshot, host: &impl Host) -> Result<(), WindowError> {
    let state = snapshot.state(|owner| host.liveness(owner));
    if state != State::Open {
        return Err(state_error(snapshot, state, host));
    }
    if let Some((index, owner)) = snapshot.unfinished() {
        return Err(format!(
            "window {} step {index} is busy: {}",
            snapshot.id,
            describe_owner(owner, host)
        )
        .into());
    }
    Ok(())
}

fn emit_warning(warning: Option<String>) {
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Closing {
    Keep,
    Abandon,
}

impl Closing {
    fn prompt(self, plan: &Snapshot) -> String {
        let n = plan.steps.len();
        let count = if n == 1 {
            "1 step".to_owned()
        } else {
            format!("{n} steps")
        };
        let id = &plan.id;
        let mut prompt = match self {
            Self::Keep => format!("ck window keep: keep window {id} ({count}) and delete its saved copies of the previous builds?"),
            Self::Abandon => format!("ck window abandon: close window {id} ({count}) without restoring anything? Its saved copies stay on disk."),
        };
        if let Some(note) = &plan.note {
            prompt.push_str(&format!(" Note: {}", report::dialog_note(note)));
        }
        prompt
    }
}

fn allow_close(
    snapshot: &Snapshot,
    dir: &Path,
    closing: Closing,
    host: &impl Host,
) -> Result<State, WindowError> {
    let state = snapshot.state(|owner| host.liveness(owner));
    match (state, closing) {
        (State::Open, _) => require_idle(snapshot, host)?,
        (State::Interrupted | State::RevertInterrupted, Closing::Abandon) => (),
        (State::Kept, Closing::Keep) if paths::info(&dir.join("saved"))?.is_some() => (),
        (State::Kept, Closing::Keep) => {
            return Err(format!("window {} is kept and cleanup is complete", snapshot.id).into())
        }
        _ => return Err(state_error(snapshot, state, host)),
    }
    Ok(state)
}
