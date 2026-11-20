//! Write-ahead log held under the install lock. A completed step is only a
//! hint: recovery also checks steps that may have acted before their done mark.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const NAME: &str = "installer.journal";
const VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Step {
    pub target: PathBuf,
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Action {
    File(FileStep),
    Bundle(FileStep),
    Symlink(FileStep),
    Inventory {
        file: FileStep,
        contents: String,
        mode: u32,
    },
    Bootstrap {
        file: FileStep,
        archive_sha256: String,
        version: String,
    },
    Activation {
        module: Option<String>,
        before: Option<Live>,
        after: Option<Build>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_intent: Option<activation::Intent>,
    },
    Registration {
        before: Registration,
        after: Registration,
    },
    UserPath {
        before: Option<UserPath>,
        after: Option<UserPath>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserPath {
    pub expandable: bool,
    pub units: Vec<u16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub registered: bool,
    pub definition: PathBuf,
    pub definition_sha256: Option<String>,
    pub program: PathBuf,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Live {
    pub identity: String,
    pub build: Build,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "proof", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Build {
    Declared {
        git_sha: String,
        lock_digest: String,
    },
    Image {
        sha256: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileStep {
    pub before: Option<String>,
    pub after: Option<String>,
    pub previous: Option<PathBuf>,
    pub staged: Option<PathBuf>,
    pub staging_directory: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Undo {
    Quiesce,
    Restore,
    Reactivate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Outcome {
    Committed,
    Undone,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    count: usize,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "kebab-case", deny_unknown_fields)]
enum Record {
    Begin {
        version: u32,
        id: String,
    },
    Step {
        step: Step,
    },
    Handover {
        step: Step,
        seal: Seal,
    },
    Seal {
        step: Step,
        seal: Seal,
    },
    Done {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reported_build: Option<Build>,
    },
    Undo {
        phase: Undo,
    },
    Undone {
        phase: Undo,
        index: usize,
    },
    Close {
        outcome: Outcome,
    },
    Cleanup {
        index: usize,
        path: PathBuf,
        identity: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum End {
    Open,
    Handover,
    Sealed,
}

pub(super) enum Status {
    Open,
    NeedsCleanup,
    Retired,
}

#[derive(Clone, Debug)]
struct State {
    id: Option<String>,
    steps: Vec<Step>,
    done: Vec<bool>,
    end: End,
    untrusted_seal: bool,
    undo: Option<Undo>,
    undo_done: Vec<bool>,
    outcome: Option<Outcome>,
    cleanup: std::collections::BTreeMap<usize, (PathBuf, String)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            id: None,
            steps: Vec::new(),
            done: Vec::new(),
            end: End::Open,
            untrusted_seal: false,
            undo: None,
            undo_done: Vec::new(),
            outcome: None,
            cleanup: std::collections::BTreeMap::new(),
        }
    }
}

impl State {
    fn add(&mut self, record: Record) -> Result<(), String> {
        if self.outcome.is_some() {
            if let Record::Cleanup {
                index,
                path,
                identity,
            } = record
            {
                if self
                    .steps
                    .get(index)
                    .and_then(|step| step.action.file())
                    .is_some_and(|file| file.staging_directory == path)
                    && !self.cleanup.contains_key(&index)
                {
                    self.cleanup.insert(index, (path, identity));
                    return Ok(());
                }
            }
            return Err("record after transaction close".into());
        }
        if self.id.is_none() && !matches!(record, Record::Begin { .. }) {
            return Err("missing transaction id".into());
        }
        match record {
            Record::Begin { version, .. } if version != VERSION => {
                return Err(format!(
                    "unsupported journal format {version}; this ck supports format {VERSION}"
                ));
            }
            Record::Begin { id, .. } if self.id.is_none() && valid_id(&id) => {
                self.id = Some(id);
            }
            Record::Step { step }
                if self.end == End::Open
                    && !self.untrusted_seal
                    && self.undo.is_none()
                    && !matches!(
                        step.action,
                        Action::Bootstrap { .. } | Action::Inventory { .. }
                    ) =>
            {
                self.push(step)?;
            }
            Record::Handover { step, seal }
                if self.steps.is_empty()
                    && self.undo.is_none()
                    && matches!(step.action, Action::Bootstrap { .. }) =>
            {
                self.push(step)?;
                self.check_seal(seal, End::Handover)?;
            }
            Record::Seal { step, seal }
                if self.end != End::Sealed
                    && !self.untrusted_seal
                    && self.undo.is_none()
                    && matches!(step.action, Action::Inventory { .. }) =>
            {
                self.push(step)?;
                self.check_seal(seal, End::Sealed)?;
            }
            Record::Done { index, .. } if self.undo.is_none() => {
                let done = self
                    .done
                    .get_mut(index)
                    .ok_or("done mark for unknown step")?;
                *done = true;
            }
            Record::Undo { phase } if next_undo(self.undo) == Some(phase) => {
                if self.undo.is_some() && self.undo_done.contains(&false) {
                    return Err("undo phase started before the previous phase finished".into());
                }
                self.undo = Some(phase);
                self.undo_done = vec![false; self.steps.len()];
            }
            Record::Undone { phase, index } if self.undo == Some(phase) => {
                let done = self
                    .undo_done
                    .get_mut(index)
                    .ok_or("undo mark for unknown step")?;
                *done = true;
            }
            Record::Close {
                outcome: Outcome::Committed,
            } if self.end == End::Sealed && self.undo.is_none() => {
                self.outcome = Some(Outcome::Committed);
            }
            Record::Close {
                outcome: Outcome::Undone,
            } if self.undo == Some(Undo::Reactivate) && !self.undo_done.contains(&false) => {
                self.outcome = Some(Outcome::Undone);
            }
            _ => return Err("unexpected record for the transaction state".into()),
        }
        Ok(())
    }

    fn push(&mut self, step: Step) -> Result<(), String> {
        if let Some(previous) = self
            .steps
            .iter()
            .rev()
            .find(|previous| previous.same_target(&step))
        {
            if !previous.followed_by(&step) {
                return Err(format!(
                    "state chain for {} does not match the previous write",
                    step.target.display()
                ));
            }
        }
        self.steps.push(step);
        self.done.push(false);
        Ok(())
    }

    fn check_seal(&mut self, seal: Seal, end: End) -> Result<(), String> {
        let actual = make_seal(&self.steps, None)?;
        if actual.count == seal.count && actual.sha256 == seal.sha256 {
            self.end = end;
        } else {
            self.end = End::Open;
            self.untrusted_seal = true;
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

impl Step {
    fn same_target(&self, other: &Self) -> bool {
        if self.target != other.target {
            return false;
        }
        match (&self.action, &other.action) {
            (Action::Activation { module: left, .. }, Action::Activation { module: right, .. }) => {
                left == right
            }
            (Action::Registration { .. }, Action::Registration { .. }) => true,
            (Action::UserPath { .. }, Action::UserPath { .. }) => true,
            (left, right) => left.file().is_some() && right.file().is_some(),
        }
    }

    fn followed_by(&self, other: &Self) -> bool {
        match (&self.action, &other.action) {
            (Action::Activation { after, .. }, Action::Activation { before, .. }) => {
                after.as_ref() == before.as_ref().map(|live| &live.build)
            }
            (Action::Registration { after, .. }, Action::Registration { before, .. }) => {
                after == before
            }
            (Action::UserPath { after, .. }, Action::UserPath { before, .. }) => after == before,
            (left, right) => left
                .file()
                .zip(right.file())
                .is_some_and(|(left, right)| left.after == right.before),
        }
    }
}

impl Action {
    pub fn file(&self) -> Option<&FileStep> {
        match self {
            Self::File(file)
            | Self::Bundle(file)
            | Self::Symlink(file)
            | Self::Inventory { file, .. }
            | Self::Bootstrap { file, .. } => Some(file),
            _ => None,
        }
    }
}

fn make_seal(steps: &[Step], last: Option<&Step>) -> Result<Seal, String> {
    let mut hash = Sha256::new();
    for step in steps.iter().chain(last) {
        let bytes = serde_json::to_vec(step).map_err(|error| error.to_string())?;
        hash.update(bytes);
        hash.update(b"\n");
    }
    Ok(Seal {
        count: steps.len() + usize::from(last.is_some()),
        sha256: format!("{:x}", hash.finalize()),
    })
}

fn next_undo(current: Option<Undo>) -> Option<Undo> {
    match current {
        None => Some(Undo::Quiesce),
        Some(Undo::Quiesce) => Some(Undo::Restore),
        Some(Undo::Restore) => Some(Undo::Reactivate),
        Some(Undo::Reactivate) => None,
    }
}

/// The caller holds the install lock for the lifetime of this handle.
#[derive(Debug)]
pub(super) struct Journal {
    path: PathBuf,
    file: File,
    state: State,
    poisoned: bool,
}

impl Journal {
    pub fn path(manifest: &Path) -> PathBuf {
        manifest.with_file_name(NAME)
    }

    pub fn exists(manifest: &Path) -> Result<bool, String> {
        let path = Self::path(manifest);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !is_reparse_point(&metadata) => Ok(true),
            Ok(_) => Err(format!(
                "refusal: {} is not a plain journal file",
                path.display()
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(journal_error(&path, error)),
        }
    }

    pub fn create(manifest: &Path) -> Result<Self, String> {
        let path = Self::path(manifest);
        let file = open_journal(&path, true).map_err(|error| journal_error(&path, error))?;
        file.sync_all()
            .map_err(|error| journal_error(&path, error))?;
        sync_parent(&path)?;
        let mut journal = Self {
            path,
            file,
            state: State::default(),
            poisoned: false,
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| journal_error(&journal.path, error))?
            .as_nanos();
        journal.append(Record::Begin {
            version: VERSION,
            id: format!("{:x}-{timestamp:x}", std::process::id()),
        })?;
        Ok(journal)
    }

    pub fn open(manifest: &Path) -> Result<Option<Self>, String> {
        if !Self::exists(manifest)? {
            return Ok(None);
        }
        let path = Self::path(manifest);
        Self::open_path(path).map(Some)
    }

    fn open_path(path: PathBuf) -> Result<Self, String> {
        let mut file = open_journal(&path, false).map_err(|error| journal_error(&path, error))?;
        let metadata = file
            .metadata()
            .map_err(|error| journal_error(&path, error))?;
        if !metadata.is_file() || is_reparse_point(&metadata) {
            return Err(journal_error(&path, "journal is not a plain file"));
        }
        verify_journal_owner(&file).map_err(|error| journal_error(&path, error))?;
        let (state, torn) = read_state(&mut file, &path)?;
        if let Some(end) = torn {
            // A partial final record could not have authorized any operation.
            file.set_len(end as u64)
                .map_err(|error| journal_error(&path, error))?;
            file.sync_all()
                .map_err(|error| journal_error(&path, error))?;
        }
        Ok(Self {
            path,
            file,
            state,
            poisoned: false,
        })
    }

    pub fn status(manifest: &Path) -> Result<Option<Status>, String> {
        let path = Self::path(manifest);
        let mut file = match subc_os::open_regular_nofollow(&path) {
            Ok(file) => file,
            Err(subc_os::OpenRegularError::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok((!retired::entries(manifest)?.is_empty()).then_some(Status::Retired))
            }
            Err(error) => return Err(journal_error(&path, error)),
        };
        verify_journal_owner(&file).map_err(|error| journal_error(&path, error))?;
        let (state, _) = read_state(&mut file, &path)?;
        Ok(Some(if state.outcome.is_some() {
            Status::NeedsCleanup
        } else {
            Status::Open
        }))
    }

    #[cfg(windows)]
    pub fn retained_removals(manifest: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
        let mut removals = Vec::new();
        for path in retired::entries(manifest)? {
            let (journal, outcome) = retired::open(path)?;
            if outcome != Outcome::Committed {
                continue;
            }
            removals.extend(journal.state.steps.iter().filter_map(|step| {
                let Action::File(file) = &step.action else {
                    return None;
                };
                let image = file.previous.as_ref()?;
                (file.after.is_none() && image.exists())
                    .then(|| (image.clone(), file.staging_directory.clone()))
            }));
        }
        Ok(removals)
    }

    pub fn prepare(&mut self, step: Step) -> Result<usize, String> {
        let index = self.state.steps.len();
        let record = match step.action {
            Action::Bootstrap { .. } => Record::Handover {
                seal: make_seal(&self.state.steps, Some(&step))?,
                step,
            },
            Action::Inventory { .. } => Record::Seal {
                seal: make_seal(&self.state.steps, Some(&step))?,
                step,
            },
            _ => Record::Step { step },
        };
        self.append(record)?;
        Ok(index)
    }

    pub fn step(&self, index: usize) -> &Step {
        &self.state.steps[index]
    }

    pub fn id(&self) -> &str {
        self.state
            .id
            .as_deref()
            .expect("only a started journal can write ahead")
    }

    pub fn next_index(&self) -> usize {
        self.state.steps.len()
    }

    pub fn handover(&self) -> Option<&Step> {
        (self.state.end == End::Handover && self.state.undo.is_none()).then(|| &self.state.steps[0])
    }

    pub fn done(&mut self, index: usize) -> Result<(), String> {
        self.done_with_build(index, None)
    }

    pub fn done_with_build(
        &mut self,
        index: usize,
        reported_build: Option<Build>,
    ) -> Result<(), String> {
        self.append(Record::Done {
            index,
            reported_build,
        })
    }

    pub fn recover(mut self, backend: &mut impl Recovery) -> Result<Outcome, String> {
        self.require_usable()?;
        if self.state.id.is_none() {
            // Creation does not authorize a change until the header is synced.
            return self.discard(backend, Outcome::Undone).map_err(String::from);
        }
        if let Some(outcome) = self.state.outcome {
            return self.discard(backend, outcome).map_err(String::from);
        }
        if self.state.end == End::Sealed
            && self.state.undo.is_none()
            && self.finish_forward(backend)?
        {
            return self.commit(backend);
        }
        let mut phase = self.state.undo.or(Some(Undo::Quiesce));
        while let Some(current) = phase {
            if self.state.undo != Some(current) {
                self.append(Record::Undo { phase: current })?;
            }
            let mut indices = (0..self.state.steps.len()).collect::<Vec<_>>();
            if current == Undo::Reactivate {
                // Restore registration before restarting the daemon, then
                // bring back its supervised modules through the control plane.
                indices.sort_by_key(|&index| match self.state.steps[index].action {
                    Action::Registration { .. } => 0,
                    Action::Activation { module: None, .. } => 1,
                    _ => 2,
                });
            } else {
                indices.reverse();
                if current == Undo::Quiesce {
                    indices.sort_by_key(|&index| {
                        !matches!(self.state.steps[index].action, Action::Activation { .. })
                    });
                }
            }
            for index in indices {
                if self.state.undo_done[index] {
                    continue;
                }
                let step = &self.state.steps[index];
                // Only the newest activation needs quiescing and only the
                // original activation state should be restored.
                let activation_handled_elsewhere = matches!(step.action, Action::Activation { .. })
                    && match current {
                        Undo::Quiesce => self.state.steps[index + 1..]
                            .iter()
                            .any(|later| step.same_target(later)),
                        Undo::Reactivate => self.state.steps[..index]
                            .iter()
                            .any(|earlier| step.same_target(earlier)),
                        Undo::Restore => false,
                    };
                if !activation_handled_elsewhere {
                    backend.undo(self.id(), current, step)?;
                }
                self.append(Record::Undone {
                    phase: current,
                    index,
                })?;
            }
            phase = next_undo(phase);
        }
        self.append(Record::Close {
            outcome: Outcome::Undone,
        })?;
        self.discard(backend, Outcome::Undone).map_err(String::from)
    }

    fn finish_forward(&mut self, backend: &mut impl Recovery) -> Result<bool, String> {
        let mut replay = Vec::new();
        for (index, step) in self.state.steps.iter().enumerate() {
            if self.state.steps[index + 1..]
                .iter()
                .any(|later| step.same_target(later))
            {
                continue;
            }
            if backend.is_applied_with_mark(step, self.state.done[index])? {
                continue;
            }
            if self.state.done[index] || !backend.can_replay(self.id(), step)? {
                return Ok(false);
            }
            // Replaying a chain is only safe from its verified before-state;
            // earlier revisions are already reflected in that before-state.
            replay.push(index);
        }
        for index in replay {
            let step = &self.state.steps[index];
            if let Err(error) = backend.replay(self.id(), step) {
                super::messages::warning(format!(
                    "could not finish install step at {}: {error}; undoing the transaction",
                    step.target.display()
                ));
                return Ok(false);
            }
            if !backend.is_applied_with_mark(step, true)? {
                return Ok(false);
            }
            self.done(index)?;
        }
        Ok(true)
    }

    pub fn commit(mut self, backend: &mut impl Recovery) -> Result<Outcome, String> {
        self.append(Record::Close {
            outcome: Outcome::Committed,
        })?;
        self.discard(backend, Outcome::Committed)
            .map_err(CleanupRefusal::after_commit)
    }

    fn discard(
        mut self,
        backend: &mut impl Recovery,
        outcome: Outcome,
    ) -> Result<Outcome, CleanupRefusal> {
        let mut busy = Vec::new();
        for index in 0..self.state.steps.len() {
            if !self.state.cleanup.contains_key(&index) {
                let step = &self.state.steps[index];
                let stage = step.action.file().map(|file| &file.staging_directory);
                if let Some(identity) = backend
                    .prepare_discard(self.id(), step, outcome)
                    .map_err(|error| CleanupRefusal::at(stage.unwrap_or(&step.target), error))?
                {
                    let path = stage
                        .ok_or_else(|| {
                            CleanupRefusal::at(&step.target, "staging cleanup without a file step")
                        })?
                        .clone();
                    self.append(Record::Cleanup {
                        index,
                        path,
                        identity,
                    })
                    .map_err(|error| CleanupRefusal::at(&self.path, error))?;
                }
            }
            if let Some((path, identity)) = self.state.cleanup.get(&index) {
                busy.extend(backend.discard(
                    self.id(),
                    &self.state.steps[index],
                    path,
                    identity,
                )?);
            }
        }
        if !busy.is_empty() {
            let path = self.path.clone();
            retired::leave(self, &busy).map_err(|error| CleanupRefusal::at(&path, error))?;
            return Ok(outcome);
        }
        drop(self.file);
        fs::remove_file(&self.path).map_err(|error| CleanupRefusal::at(&self.path, error))?;
        sync_parent(&self.path).map_err(|error| CleanupRefusal::at(&self.path, error))?;
        Ok(outcome)
    }

    fn append(&mut self, record: Record) -> Result<(), String> {
        self.append_with(record, |file, bytes| {
            file.write_all(bytes).and_then(|()| file.sync_all())
        })
    }

    fn append_with(
        &mut self,
        record: Record,
        persist: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> Result<(), String> {
        self.require_usable()?;
        let mut bytes =
            serde_json::to_vec(&record).map_err(|error| journal_error(&self.path, error))?;
        let mut next = self.state.clone();
        next.add(record)
            .map_err(|error| journal_error(&self.path, error))?;
        bytes.push(b'\n');
        let previous = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|error| journal_error(&self.path, error))?;
        if let Err(error) = persist(&mut self.file, &bytes) {
            self.poisoned = true;
            if let Err(reset) = self
                .file
                .set_len(previous)
                .and_then(|()| self.file.sync_all())
            {
                return Err(journal_error(
                    &self.path,
                    format!("{error}; could not restore the previous journal length: {reset}"),
                ));
            }
            return Err(journal_error(&self.path, error));
        }
        self.state = next;
        Ok(())
    }

    fn require_usable(&self) -> Result<(), String> {
        if self.poisoned {
            return Err(journal_error(
                &self.path,
                "previous append failed; reopen the journal before recovering",
            ));
        }
        Ok(())
    }
}

pub(super) trait Recovery {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String>;
    fn is_applied_with_mark(&mut self, step: &Step, _done: bool) -> Result<bool, String> {
        self.is_applied(step)
    }
    /// Check the recorded old state and that the new state can be replayed.
    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String>;
    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String>;
    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String>;
    fn prepare_discard(
        &mut self,
        id: &str,
        step: &Step,
        _outcome: Outcome,
    ) -> Result<Option<String>, String> {
        files::prepare_discard(id, step)
    }
    fn discard(
        &mut self,
        id: &str,
        step: &Step,
        path: &Path,
        identity: &str,
    ) -> Result<Vec<PathBuf>, CleanupRefusal> {
        files::discard(id, step, path, identity)
    }
}

#[derive(Debug)]
pub(super) struct CleanupRefusal {
    path: PathBuf,
    detail: String,
}

impl CleanupRefusal {
    fn at(path: &Path, detail: impl std::fmt::Display) -> Self {
        Self {
            path: path.to_owned(),
            detail: detail.to_string(),
        }
    }

    fn after_commit(self) -> String {
        format!("installed and recorded, but cleanup of {} was refused: {}; inspect or remove that file, then rerun", self.path.display(), self.detail)
    }
}

impl From<CleanupRefusal> for String {
    fn from(refusal: CleanupRefusal) -> Self {
        format!(
            "refusal: install journal cleanup {}: {}",
            refusal.path.display(),
            refusal.detail
        )
    }
}

fn journal_error(path: &Path, error: impl std::fmt::Display) -> String {
    format!("refusal: install journal {}: {error}", path.display())
}

fn read_state(file: &mut File, path: &Path) -> Result<(State, Option<usize>), String> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(file, &mut bytes).map_err(|error| journal_error(path, error))?;
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut state = State::default();
    for line in bytes[..end].split_inclusive(|byte| *byte == b'\n') {
        let record = serde_json::from_slice(line).map_err(|error| journal_error(path, error))?;
        state
            .add(record)
            .map_err(|error| journal_error(path, error))?;
    }
    Ok((state, (end != bytes.len()).then_some(end)))
}

#[cfg(unix)]
fn open_journal(path: &Path, create: bool) -> io::Result<File> {
    use rustix::fs::{open, Mode, OFlags};
    let mut flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if create {
        flags |= OFlags::CREATE | OFlags::EXCL;
    }
    Ok(File::from(open(path, flags, Mode::RUSR | Mode::WUSR)?))
}

#[cfg(windows)]
fn open_journal(path: &Path, create: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    if create {
        return subc_os::windows_acl::create_private_file(path);
    }
    // FILE_FLAG_OPEN_REPARSE_POINT; a link must not redirect the journal.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x0020_0000)
        .open(path)
}

#[cfg(unix)]
fn verify_journal_owner(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let info = file.metadata()?;
    if info.uid() != rustix::process::geteuid().as_raw()
        || info.mode() & 0o077 != 0
        || info.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "journal must be owner-only with exactly one name",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn verify_journal_owner(file: &File) -> io::Result<()> {
    subc_os::windows_acl::verify_owner_only(file)
}

pub(super) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests;

pub(super) mod activation;
pub(super) mod bootstrap;
#[cfg(unix)]
pub(super) mod bundle;
pub(super) mod files;
mod legacy;
#[cfg(unix)]
pub(super) mod profile;
pub(super) mod registration;
pub(super) mod retired;
#[cfg(unix)]
pub(super) mod symlink;
pub(super) mod system;
pub(super) mod transaction;
#[cfg(windows)]
pub(super) mod user_path;
pub use bootstrap::entry as bootstrap_entry;

pub(super) fn sync_parent(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| journal_error(path, error))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(feature = "test-support")]
pub(in crate::setup) fn crash_hook(point: &str) {
    if std::env::var("CK_TEST_INSTALL_CRASH_POINT").as_deref() != Ok(point) {
        return;
    }
    if let Some(marker) = std::env::var_os("CK_TEST_INSTALL_CRASH_MARKER") {
        fs::write(marker, point).expect("write installer crash marker");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
