//! Hermetic repository and seam fakes shared by `cortexkit-release` integration tests.
//!
//! Integration tests include this module with `mod support;`. The helper mints
//! every repository at runtime and keeps each temporary directory alive for the
//! duration of the test; it never copies or stages a `.git` directory. Diverged
//! remotes are local bare repositories, so this helper never contacts a public
//! release service.

#![allow(dead_code)]

use cortexkit_release::{
    ApprovalGate, ApprovalSubject, ApprovalToken, CompletionProbe, DurableState, DurableWrite,
    EffectRequest, IrreversibleExecutor, ProbeEvidence, ProbeResult, SeamError,
};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

/// The Git and durable-state shape to mint for one test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryShape {
    Valid,
    DirtyTree,
    DivergedRemote,
    SiblingCheckoutDrift,
    StaleWorkingTreeResidue,
    RuntimeResidueFiles,
    MissingDeclaration,
    TornJournalTail,
}

/// A separately minted checkout whose HEAD moved after the recorded pin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiblingCheckoutDrift {
    pub path: PathBuf,
    pub pinned_commit: String,
    pub current_commit: String,
}

struct MintedSibling {
    directory: TempDir,
    checkout: SiblingCheckoutDrift,
}

/// A runtime-minted repository and a separate temporary home for durable state.
pub struct MintedRepo {
    repository: TempDir,
    journal_home: TempDir,
    remote: Option<TempDir>,
    sibling: Option<MintedSibling>,
    residue_paths: Vec<PathBuf>,
}

impl MintedRepo {
    /// Mints a repository with the requested shape using the default declaration.
    pub fn mint(shape: RepositoryShape) -> io::Result<Self> {
        Self::mint_with_declaration(shape, default_declaration())
    }

    /// Mints a repository with a caller-supplied release declaration.
    pub fn mint_with_declaration(
        shape: RepositoryShape,
        declaration: impl AsRef<str>,
    ) -> io::Result<Self> {
        let repository = tempfile::tempdir()?;
        let journal_home = tempfile::tempdir()?;
        let root = repository.path();

        run_git(root, ["init"])?;
        run_git(root, ["config", "user.name", "ck-release test"])?;
        run_git(
            root,
            ["config", "user.email", "ck-release-test@example.invalid"],
        )?;
        fs::create_dir_all(root.join(".cortexkit"))?;
        fs::write(root.join("README.md"), "minted test repository\n")?;
        fs::write(root.join(".cortexkit/release.jsonc"), declaration.as_ref())?;
        commit_baseline(root)?;

        let mut minted = Self {
            repository,
            journal_home,
            remote: None,
            sibling: None,
            residue_paths: Vec::new(),
        };

        match shape {
            RepositoryShape::Valid => {}
            RepositoryShape::DirtyTree => minted.make_dirty_tree()?,
            RepositoryShape::DivergedRemote => minted.make_diverged_remote()?,
            RepositoryShape::SiblingCheckoutDrift => minted.make_sibling_checkout_drift()?,
            RepositoryShape::StaleWorkingTreeResidue => minted.make_stale_working_tree_residue()?,
            RepositoryShape::RuntimeResidueFiles => minted.make_runtime_residue_files()?,
            RepositoryShape::MissingDeclaration => {
                fs::remove_file(minted.declaration_path())?;
            }
            RepositoryShape::TornJournalTail => minted.write_torn_journal_tail()?,
        }

        Ok(minted)
    }

    /// Returns the root of the Git working tree.
    pub fn path(&self) -> &Path {
        self.repository.path()
    }

    /// Returns the declaration path in the minted repository.
    pub fn declaration_path(&self) -> PathBuf {
        self.path().join(".cortexkit/release.jsonc")
    }

    /// Returns a per-test durable-state home that is outside the repository tree.
    pub fn journal_home(&self) -> &Path {
        self.journal_home.path()
    }

    /// Returns the path used by the torn-tail fixture.
    pub fn journal_path(&self) -> PathBuf {
        self.journal_home()
            .join("release/test-repository/test-train.journal")
    }

    /// Returns a sibling checkout whose current commit differs from its recorded pin.
    pub fn sibling_checkout_drift(&self) -> Option<&SiblingCheckoutDrift> {
        self.sibling.as_ref().map(|sibling| &sibling.checkout)
    }

    /// Returns the files that represent incomplete state left by a prior run.
    pub fn residue_paths(&self) -> &[PathBuf] {
        &self.residue_paths
    }

    /// Writes a custom declaration without hiding that the working tree changed.
    pub fn write_declaration(&self, declaration: impl AsRef<str>) -> io::Result<()> {
        fs::create_dir_all(
            self.declaration_path()
                .parent()
                .expect("declaration has parent"),
        )?;
        fs::write(self.declaration_path(), declaration.as_ref())
    }

    /// Creates an incomplete final JSONL record outside the repository tree.
    pub fn write_torn_journal_tail(&self) -> io::Result<()> {
        let journal = self.journal_path();
        fs::create_dir_all(journal.parent().expect("journal has parent"))?;
        fs::write(
            journal,
            b"{\"format\":1,\"event\":\"train-start\"}\n{\"format\":1,\"event\":",
        )
    }

    fn make_dirty_tree(&mut self) -> io::Result<()> {
        fs::write(
            self.path().join("README.md"),
            "minted test repository\ndirty change\n",
        )
    }

    fn make_sibling_checkout_drift(&mut self) -> io::Result<()> {
        let sibling = tempfile::tempdir()?;
        let path = sibling.path();
        run_git(path, ["init"])?;
        run_git(path, ["config", "user.name", "ck-release test"])?;
        run_git(
            path,
            ["config", "user.email", "ck-release-test@example.invalid"],
        )?;
        fs::write(path.join("API.md"), "api=v1\n")?;
        run_git(path, ["add", "API.md"])?;
        run_git(path, ["commit", "-m", "pinned sibling API"])?;
        let pinned_commit = git_stdout(path, ["rev-parse", "HEAD"])?;

        fs::write(path.join("API.md"), "api=v2\n")?;
        run_git(path, ["add", "API.md"])?;
        run_git(path, ["commit", "-m", "sibling API drift"])?;
        let current_commit = git_stdout(path, ["rev-parse", "HEAD"])?;
        let checkout_path = path.to_path_buf();
        self.sibling = Some(MintedSibling {
            directory: sibling,
            checkout: SiblingCheckoutDrift {
                path: checkout_path,
                pinned_commit,
                current_commit,
            },
        });
        Ok(())
    }

    fn make_stale_working_tree_residue(&mut self) -> io::Result<()> {
        let version_bump = self.path().join("VERSION");
        let lockfile = self.path().join("Cargo.lock");
        fs::write(&version_bump, "0.40.5-aborted\n")?;
        fs::write(&lockfile, "# incomplete lockfile from aborted release\n")?;
        self.residue_paths = vec![version_bump, lockfile];
        Ok(())
    }

    fn make_runtime_residue_files(&mut self) -> io::Result<()> {
        let process = self
            .path()
            .join(".cortexkit/release-residue/process-1234.pid");
        let port = self
            .path()
            .join(".cortexkit/release-residue/port-49152.lock");
        let temporary_root = self.path().join("target/release-residue/session.tmp");
        for path in [&process, &port, &temporary_root] {
            fs::create_dir_all(path.parent().expect("residue path has a parent"))?;
            fs::write(path, "left by interrupted release\n")?;
        }
        self.residue_paths = vec![process, port, temporary_root];
        Ok(())
    }

    fn make_diverged_remote(&mut self) -> io::Result<()> {
        let remote = tempfile::tempdir()?;
        let remote_path = remote.path();
        run_git(remote_path, ["init", "--bare"])?;

        let branch = git_stdout(self.path(), ["branch", "--show-current"])?;
        let base_commit = git_stdout(self.path(), ["rev-parse", "HEAD"])?;
        run_git(
            self.path(),
            [
                "remote",
                "add",
                "origin",
                remote_path.to_string_lossy().as_ref(),
            ],
        )?;
        run_git(
            self.path(),
            ["push", "--set-upstream", "origin", branch.as_str()],
        )?;

        fs::write(
            self.path().join("README.md"),
            "minted test repository\nlocal divergence\n",
        )?;
        run_git(self.path(), ["add", "README.md"])?;
        run_git(self.path(), ["commit", "-m", "local divergence"])?;

        let base_tree = git_stdout(self.path(), ["rev-parse", "HEAD~1^{tree}"])?;
        let remote_commit = git_in_git_dir(
            remote_path,
            [
                "commit-tree",
                base_tree.as_str(),
                "-p",
                base_commit.as_str(),
                "-m",
                "remote divergence",
            ],
        )?;
        git_in_git_dir(
            remote_path,
            [
                "update-ref",
                &format!("refs/heads/{branch}"),
                remote_commit.as_str(),
            ],
        )?;
        run_git(self.path(), ["fetch", "origin"])?;

        self.remote = Some(remote);
        Ok(())
    }
}

/// A trace entry emitted by one recording fake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordedCall {
    Probe(EffectRequest),
    Execute(EffectRequest),
    DurableAppend(DurableWrite),
    Approval(ApprovalSubject),
}

/// The class of a call that must not occur in a test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallKind {
    Probe,
    Execute,
    DurableAppend,
    Approval,
}

/// Shared trace storage for fakes that must prove cross-seam call order.
#[derive(Clone, Debug, Default)]
pub struct CallRecorder {
    inner: Arc<Mutex<RecordedState>>,
}

#[derive(Clone, Debug, Default)]
struct RecordedState {
    calls: Vec<RecordedCall>,
    durable_writes: Vec<DurableWrite>,
    irreversible_effects: Vec<EffectRequest>,
}

impl CallRecorder {
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.with_state(|state| state.calls.clone())
    }

    pub fn durable_writes(&self) -> Vec<DurableWrite> {
        self.with_state(|state| state.durable_writes.clone())
    }

    pub fn irreversible_effects(&self) -> Vec<EffectRequest> {
        self.with_state(|state| state.irreversible_effects.clone())
    }

    /// Panics unless every observed call exactly matches the supplied order.
    pub fn assert_call_order(&self, expected: &[RecordedCall]) {
        assert_eq!(self.calls(), expected, "unexpected fake call sequence");
    }

    /// Panics if any call of a forbidden class was recorded.
    pub fn assert_no_forbidden_calls(&self, forbidden: &[CallKind]) {
        let calls = self.calls();
        let unexpected = calls.iter().find(|call| {
            forbidden.iter().any(|kind| {
                matches!(
                    (kind, call),
                    (CallKind::Probe, RecordedCall::Probe(_))
                        | (CallKind::Execute, RecordedCall::Execute(_))
                        | (CallKind::DurableAppend, RecordedCall::DurableAppend(_))
                        | (CallKind::Approval, RecordedCall::Approval(_))
                )
            })
        });
        assert!(
            unexpected.is_none(),
            "forbidden fake call recorded: {unexpected:?}"
        );
    }

    fn record(&self, call: RecordedCall) {
        self.with_state_mut(|state| state.calls.push(call));
    }

    fn record_durable_write(&self, write: DurableWrite) {
        self.with_state_mut(|state| state.durable_writes.push(write));
    }

    fn record_irreversible_effect(&self, request: EffectRequest) {
        self.with_state_mut(|state| state.irreversible_effects.push(request));
    }

    fn with_state<T>(&self, operation: impl FnOnce(&RecordedState) -> T) -> T {
        let state = self.inner.lock().expect("recording fake mutex poisoned");
        operation(&state)
    }

    fn with_state_mut<T>(&self, operation: impl FnOnce(&mut RecordedState) -> T) -> T {
        let mut state = self.inner.lock().expect("recording fake mutex poisoned");
        operation(&mut state)
    }
}

/// A scripted completion probe that records every request.
pub struct RecordingProbe {
    recorder: CallRecorder,
    outcomes: VecDeque<Result<ProbeResult, SeamError>>,
}

impl RecordingProbe {
    pub fn new(
        recorder: CallRecorder,
        outcomes: impl IntoIterator<Item = Result<ProbeResult, SeamError>>,
    ) -> Self {
        Self {
            recorder,
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl CompletionProbe for RecordingProbe {
    fn probe(&mut self, request: &EffectRequest) -> Result<ProbeResult, SeamError> {
        self.recorder.record(RecordedCall::Probe(request.clone()));
        self.outcomes
            .pop_front()
            .unwrap_or_else(|| Err(SeamError::new("recording probe has no scripted outcome")))
    }
}

/// A scripted irreversible executor that records effects only after success.
pub struct RecordingExecutor {
    recorder: CallRecorder,
    outcomes: VecDeque<Result<ProbeEvidence, SeamError>>,
}

impl RecordingExecutor {
    pub fn new(
        recorder: CallRecorder,
        outcomes: impl IntoIterator<Item = Result<ProbeEvidence, SeamError>>,
    ) -> Self {
        Self {
            recorder,
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl IrreversibleExecutor for RecordingExecutor {
    fn execute(&mut self, request: &EffectRequest) -> Result<ProbeEvidence, SeamError> {
        self.recorder.record(RecordedCall::Execute(request.clone()));
        let outcome = self
            .outcomes
            .pop_front()
            .unwrap_or_else(|| Err(SeamError::new("recording executor has no scripted outcome")));
        if outcome.is_ok() {
            self.recorder.record_irreversible_effect(request.clone());
        }
        outcome
    }
}

/// A durable-state fake that retains exact requested journal writes.
pub struct RecordingDurableState {
    recorder: CallRecorder,
    outcomes: VecDeque<Result<(), SeamError>>,
}

impl RecordingDurableState {
    pub fn new(
        recorder: CallRecorder,
        outcomes: impl IntoIterator<Item = Result<(), SeamError>>,
    ) -> Self {
        Self {
            recorder,
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl DurableState for RecordingDurableState {
    fn append(&mut self, write: &DurableWrite) -> Result<(), SeamError> {
        self.recorder
            .record(RecordedCall::DurableAppend(write.clone()));
        let outcome = self.outcomes.pop_front().unwrap_or(Ok(()));
        if outcome.is_ok() {
            self.recorder.record_durable_write(write.clone());
        }
        outcome
    }
}

/// An approval fake that records the exact subject before returning a token.
pub struct RecordingApprovalGate {
    recorder: CallRecorder,
    outcomes: VecDeque<Result<ApprovalToken, SeamError>>,
}

impl RecordingApprovalGate {
    pub fn new(
        recorder: CallRecorder,
        outcomes: impl IntoIterator<Item = Result<ApprovalToken, SeamError>>,
    ) -> Self {
        Self {
            recorder,
            outcomes: outcomes.into_iter().collect(),
        }
    }
}

impl ApprovalGate for RecordingApprovalGate {
    fn approve(&mut self, subject: &ApprovalSubject) -> Result<ApprovalToken, SeamError> {
        self.recorder
            .record(RecordedCall::Approval(subject.clone()));
        self.outcomes.pop_front().unwrap_or_else(|| {
            Err(SeamError::new(
                "recording approval gate has no scripted outcome",
            ))
        })
    }
}

fn default_declaration() -> &'static str {
    "{\n  \"version\": 1,\n  \"trains\": []\n}\n"
}

fn commit_baseline(root: &Path) -> io::Result<()> {
    // The staged path list is explicit so test setup can never stage `.git`.
    run_git(root, ["add", "README.md", ".cortexkit/release.jsonc"])?;
    run_git(root, ["commit", "-m", "mint test baseline"])
}

fn run_git<I, S>(root: &Path, arguments: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()?;
    command_result(output).map(|_| ())
}

fn git_stdout<I, S>(root: &Path, arguments: I) -> io::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()?;
    String::from_utf8(command_result(output)?)
        .map(|value| value.trim().to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn git_in_git_dir<I, S>(git_dir: &Path, arguments: I) -> io::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "ck-release test")
        .env("GIT_AUTHOR_EMAIL", "ck-release-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "ck-release test")
        .env("GIT_COMMITTER_EMAIL", "ck-release-test@example.invalid")
        .output()?;
    String::from_utf8(command_result(output)?)
        .map(|value| value.trim().to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn command_result(output: Output) -> io::Result<Vec<u8>> {
    if output.status.success() {
        return Ok(output.stdout);
    }

    Err(io::Error::other(format!(
        "git command failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_release::{
        ArtifactId, CommitId, DeclarationDigest, EffectRequest, OperationId, PhaseInstanceId,
        RepositoryId, TrainId,
    };

    fn request() -> EffectRequest {
        EffectRequest {
            repository: RepositoryId::new("minted-repository"),
            train: TrainId::new("test-train"),
            phase: PhaseInstanceId::new("publish-assets"),
            artifact: ArtifactId::new("release-archive"),
            operation: OperationId::new("opaque-operation"),
            intended_commit: CommitId::new("abc123"),
            declaration_digest: DeclarationDigest::new("digest"),
        }
    }

    #[test]
    fn minting_shapes_and_recording_fakes_are_hermetic() {
        let valid = MintedRepo::mint(RepositoryShape::Valid).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(valid.path())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(status.stdout.is_empty());

        let dirty = MintedRepo::mint(RepositoryShape::DirtyTree).unwrap();
        assert_ne!(
            fs::read_to_string(dirty.path().join("README.md")).unwrap(),
            "minted test repository\n"
        );

        let diverged = MintedRepo::mint(RepositoryShape::DivergedRemote).unwrap();
        let divergence = Command::new("git")
            .arg("-C")
            .arg(diverged.path())
            .args(["status", "--porcelain=v2", "--branch"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&divergence.stdout).contains("branch.ab +1 -1"));

        let missing = MintedRepo::mint(RepositoryShape::MissingDeclaration).unwrap();
        assert!(!missing.declaration_path().exists());

        let torn = MintedRepo::mint(RepositoryShape::TornJournalTail).unwrap();
        assert!(fs::read(torn.journal_path())
            .unwrap()
            .ends_with(b"\"event\":"));
        assert!(!torn.journal_path().starts_with(torn.path()));

        let recorder = CallRecorder::default();
        let operation = request();
        let write = DurableWrite {
            stream: "intent".to_owned(),
            bytes: b"intent bytes".to_vec(),
        };
        let evidence = ProbeEvidence {
            reference: "fake-reference".to_owned(),
            identity: "abc123".to_owned(),
        };
        let mut probe = RecordingProbe::new(
            recorder.clone(),
            [Ok(ProbeResult::Absent(evidence.clone()))],
        );
        let mut durable = RecordingDurableState::new(recorder.clone(), [Ok(())]);
        let mut executor = RecordingExecutor::new(recorder.clone(), [Ok(evidence)]);

        assert!(matches!(
            probe.probe(&operation).unwrap(),
            ProbeResult::Absent(_)
        ));
        durable.append(&write).unwrap();
        executor.execute(&operation).unwrap();

        recorder.assert_call_order(&[
            RecordedCall::Probe(operation.clone()),
            RecordedCall::DurableAppend(write.clone()),
            RecordedCall::Execute(operation.clone()),
        ]);
        assert_eq!(recorder.durable_writes(), vec![write]);
        assert_eq!(recorder.irreversible_effects(), vec![operation]);
        recorder.assert_no_forbidden_calls(&[CallKind::Approval]);
    }
}
