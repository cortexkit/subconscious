//! Durable, append-only state streams for release trains.
//!
//! A train's journal and intent streams share an identity and live outside the
//! working repository. The intent stream is write-ahead state: callers must use
//! [`JournalStore::execute_with_intent`] for irreversible effects so a synced
//! pending intent exists before the executor can run.

use crate::{
    declaration::ParsedDeclaration, ApprovalSubject, ArtifactId, CommitId, DeclarationDigest,
    EffectRequest, IrreversibleExecutor, OperationId, PhaseInstanceId, ProbeEvidence, RepositoryId,
    SeamError, TrainId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

/// The only persisted stream format accepted by this version of the machine.
pub const STREAM_FORMAT_VERSION: u32 = 1;

/// Returns the production durable-state root without consulting a repository path.
pub fn default_state_root() -> Result<PathBuf, StateError> {
    let home = env::var_os("HOME").ok_or(StateError::HomeDirectoryUnavailable)?;
    Ok(PathBuf::from(home).join(".local/share/cortexkit/release"))
}

/// Names the two durable streams belonging to one release train.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrainJournalIdentity {
    pub repository: RepositoryId,
    pub train: TrainId,
    pub id: String,
}

impl TrainJournalIdentity {
    /// Creates an identity after rejecting values that could escape their state partition.
    pub fn new(
        repository: RepositoryId,
        train: TrainId,
        id: impl Into<String>,
    ) -> Result<Self, StateError> {
        let id = id.into();
        for value in [repository.as_str(), train.as_str(), id.as_str()] {
            validate_path_component(value)?;
        }
        Ok(Self {
            repository,
            train,
            id,
        })
    }

    /// Returns the `<train>-<id>` basename shared by journal and intent streams.
    pub fn file_stem(&self) -> String {
        format!("{}-{}", self.train, self.id)
    }
}

/// A journal and its write-ahead intent stream under one configured durable-state root.
#[derive(Clone, Debug)]
pub struct JournalStore {
    state_root: PathBuf,
    identity: TrainJournalIdentity,
}

impl JournalStore {
    /// Opens the durable streams for one train, creating only directories below `state_root`.
    pub fn new(
        state_root: impl Into<PathBuf>,
        identity: TrainJournalIdentity,
    ) -> Result<Self, StateError> {
        let store = Self {
            state_root: state_root.into(),
            identity,
        };
        fs::create_dir_all(store.repository_dir()).map_err(|source| StateError::Io {
            path: store.repository_dir(),
            source,
        })?;
        Ok(store)
    }

    /// Opens the durable streams rooted beneath the current user's local data directory.
    pub fn in_default_location(identity: TrainJournalIdentity) -> Result<Self, StateError> {
        Self::new(default_state_root()?, identity)
    }

    /// Returns the append-only journal path.
    pub fn journal_path(&self) -> PathBuf {
        self.repository_dir()
            .join(format!("{}.journal", self.identity.file_stem()))
    }

    /// Returns the append-only write-ahead intent stream path.
    pub fn intent_path(&self) -> PathBuf {
        self.repository_dir()
            .join(format!("{}.intent", self.identity.file_stem()))
    }

    /// Appends and synchronizes a journal record before returning.
    pub fn append_journal(&self, record: JournalRecord) -> Result<(), StateError> {
        append_record(&self.journal_path(), record)
    }

    /// Returns the durable `<train>-<id>` identifier used by operator ceremonies.
    pub fn train_journal_id(&self) -> String {
        self.identity.file_stem()
    }

    /// Returns the declaration currently bound to this train, if train creation pinned one.
    pub fn pinned_declaration(&self) -> Result<Option<DeclarationBinding>, StateError> {
        let mut binding = None;
        for record in self.read_journal()? {
            match record {
                JournalRecord::DeclarationPinned { binding: candidate } => {
                    binding = Some(candidate)
                }
                JournalRecord::DeclarationRebound {
                    replacement: candidate,
                    ..
                } => binding = Some(*candidate),
                JournalRecord::Completion { .. } | JournalRecord::Terminalized { .. } => {}
            }
        }
        Ok(binding)
    }

    /// Pins the active normalized declaration when the train is first created.
    pub fn pin_declaration(
        &self,
        declaration: &ParsedDeclaration,
    ) -> Result<DeclarationBinding, StateError> {
        self.ensure_active()?;
        if let Some(binding) = self.pinned_declaration()? {
            if binding.digest == declaration.digest {
                return Ok(binding);
            }
            return Err(StateError::DeclarationDigestMismatch {
                pinned: binding.digest.to_string(),
                active: declaration.digest.to_string(),
            });
        }

        let binding = DeclarationBinding {
            digest: declaration.digest.clone(),
            normalized: declaration.normalized.clone(),
        };
        self.append_journal(JournalRecord::DeclarationPinned {
            binding: binding.clone(),
        })?;
        Ok(binding)
    }

    /// Refuses mutation unless the active declaration digest equals the durable pin.
    pub fn ensure_declaration_digest(
        &self,
        active_digest: &DeclarationDigest,
    ) -> Result<(), StateError> {
        self.ensure_active()?;
        let binding = self
            .pinned_declaration()?
            .ok_or(StateError::DeclarationNotPinned)?;
        if binding.digest != *active_digest {
            return Err(StateError::DeclarationDigestMismatch {
                pinned: binding.digest.to_string(),
                active: active_digest.to_string(),
            });
        }
        Ok(())
    }

    /// Returns the terminal journal state, if an operator has abandoned the train.
    pub fn terminal_state(&self) -> Result<Option<TrainTerminalState>, StateError> {
        Ok(self
            .read_journal()?
            .into_iter()
            .find_map(|record| match record {
                JournalRecord::Terminalized { state } => Some(state),
                JournalRecord::DeclarationPinned { .. }
                | JournalRecord::DeclarationRebound { .. }
                | JournalRecord::Completion { .. } => None,
            }))
    }

    /// Appends the durable abandonment terminal record without deleting earlier evidence.
    pub fn abandon(&self) -> Result<TrainTerminalState, StateError> {
        self.ensure_active()?;
        let binding = self
            .pinned_declaration()?
            .ok_or(StateError::DeclarationNotPinned)?;
        let state = TrainTerminalState::Abandoned {
            declaration_digest: binding.digest,
        };
        self.append_journal(JournalRecord::Terminalized {
            state: state.clone(),
        })?;
        Ok(state)
    }

    /// Replaces the bound declaration after an operator has reviewed and confirmed the change.
    pub fn rebind_declaration(
        &self,
        expected_pinned_digest: &DeclarationDigest,
        replacement: &ParsedDeclaration,
    ) -> Result<DeclarationBinding, StateError> {
        self.ensure_active()?;
        let current = self
            .pinned_declaration()?
            .ok_or(StateError::DeclarationNotPinned)?;
        if current.digest != *expected_pinned_digest {
            return Err(StateError::DeclarationBindingChanged {
                expected: expected_pinned_digest.to_string(),
                actual: current.digest.to_string(),
            });
        }
        if current.digest == replacement.digest {
            return Err(StateError::DeclarationDigestUnchanged {
                digest: current.digest.to_string(),
            });
        }

        let binding = DeclarationBinding {
            digest: replacement.digest.clone(),
            normalized: replacement.normalized.clone(),
        };
        self.append_journal(JournalRecord::DeclarationRebound {
            previous_digest: current.digest,
            replacement: Box::new(binding.clone()),
        })?;
        Ok(binding)
    }

    /// Refuses public train mutation after an abandonment record has terminalized the journal.
    pub fn ensure_active(&self) -> Result<(), StateError> {
        if let Some(state) = self.terminal_state()? {
            return Err(StateError::TrainTerminal {
                state: state.to_string(),
            });
        }
        Ok(())
    }

    /// Appends and synchronizes a pending intent before an irreversible call.
    pub fn append_intent(
        &self,
        request: &EffectRequest,
        approval_subject: ApprovalSubject,
    ) -> Result<PendingIntent, StateError> {
        self.ensure_declaration_digest(&request.declaration_digest)?;
        if request.repository != self.identity.repository || request.train != self.identity.train {
            return Err(StateError::IntentDoesNotMatchTrain);
        }

        let sequence = self.read_intents()?.len() as u64 + 1;
        let intent = PendingIntent {
            sequence,
            train: request.train.clone(),
            phase: request.phase.clone(),
            artifact: request.artifact.clone(),
            operation: request.operation.clone(),
            intended_commit: request.intended_commit.clone(),
            declaration_digest: request.declaration_digest.clone(),
            approval_subject,
        };
        append_record(&self.intent_path(), IntentRecord::Pending(intent.clone()))?;
        Ok(intent)
    }

    /// Persists a write-ahead intent, then and only then invokes the irreversible executor.
    ///
    /// A returned executor error deliberately leaves the pending intent durable. Resume must
    /// reconcile that intent instead of treating a missing completion as permission to retry.
    pub fn execute_with_intent<E: IrreversibleExecutor>(
        &self,
        request: &EffectRequest,
        approval_subject: ApprovalSubject,
        executor: &mut E,
    ) -> Result<ProbeEvidence, StateError> {
        self.append_intent(request, approval_subject)?;
        executor.execute(request).map_err(StateError::Executor)
    }

    /// Appends completion after reconciliation supplies matching evidence.
    pub fn append_completion(
        &self,
        intent: &PendingIntent,
        evidence: ProbeEvidence,
    ) -> Result<(), StateError> {
        self.ensure_active()?;
        append_record(
            &self.journal_path(),
            JournalRecord::Completion {
                intent: Box::new(intent.clone()),
                evidence: Box::new(evidence),
            },
        )
    }

    /// Reads all verified journal records and reports an incomplete final record separately.
    pub fn read_journal(&self) -> Result<Vec<JournalRecord>, StateError> {
        read_records(&self.journal_path())
    }

    /// Reads all verified write-ahead intent records.
    pub fn read_intents(&self) -> Result<Vec<IntentRecord>, StateError> {
        read_records(&self.intent_path())
    }

    /// Returns durable pending intents; no record means the operation was never attempted.
    pub fn pending_intents(&self) -> Result<Vec<PendingIntent>, StateError> {
        let intents = self.read_intents()?;
        let completions = self.read_journal()?;
        Ok(intents
            .into_iter()
            .map(|record| match record {
                IntentRecord::Pending(intent) => intent,
            })
            .filter(|intent| {
                !completions.iter().any(|record| match record {
                    JournalRecord::Completion {
                        intent: completed, ..
                    } => completed.as_ref() == intent,
                    JournalRecord::DeclarationPinned { .. }
                    | JournalRecord::DeclarationRebound { .. }
                    | JournalRecord::Terminalized { .. } => false,
                })
            })
            .collect())
    }

    /// Truncates only an incomplete final record, preserving all verified records.
    pub fn recover_torn_journal_tail(&self) -> Result<Vec<JournalRecord>, StateError> {
        recover_torn_tail(&self.journal_path())
    }

    /// Truncates only an incomplete final intent record, preserving all verified records.
    pub fn recover_torn_intent_tail(&self) -> Result<Vec<IntentRecord>, StateError> {
        recover_torn_tail(&self.intent_path())
    }

    fn repository_dir(&self) -> PathBuf {
        self.state_root.join(self.identity.repository.as_str())
    }
}

/// The normalized declaration bound to a train journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeclarationBinding {
    pub digest: DeclarationDigest,
    pub normalized: Value,
}

/// The terminal state that prevents a train journal from being resumed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrainTerminalState {
    /// The operator deliberately stopped this train and retained its journal as evidence.
    Abandoned {
        declaration_digest: DeclarationDigest,
    },
}

impl std::fmt::Display for TrainTerminalState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Abandoned { .. } => formatter.write_str("abandoned"),
        }
    }
}

/// A versioned journal record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    /// The declaration pinned when the train was first created.
    DeclarationPinned { binding: DeclarationBinding },
    /// A confirmed ceremony replaced the pinned declaration. Boxed: the
    /// binding carries the full normalized declaration, dwarfing the other
    /// variants (clippy large_enum_variant on the journal's common type).
    DeclarationRebound {
        previous_digest: DeclarationDigest,
        replacement: Box<DeclarationBinding>,
    },
    /// An operator terminalized the journal without deleting its earlier evidence.
    Terminalized { state: TrainTerminalState },
    /// Reconciliation confirmed that one write-ahead intent has completed.
    /// Boxed for the same reason as the rebind variant: intent and evidence
    /// together dwarf the marker variants of this common journal type.
    Completion {
        intent: Box<PendingIntent>,
        evidence: Box<ProbeEvidence>,
    },
}

/// A versioned record in the write-ahead intent stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntentRecord {
    /// An irreversible operation may have been invoked and must be reconciled before retry.
    Pending(PendingIntent),
}

/// The exact subject durable before an irreversible per-artifact call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingIntent {
    /// Monotonic per-stream identity that prevents one completion from resolving another intent.
    pub sequence: u64,
    pub train: TrainId,
    pub phase: PhaseInstanceId,
    pub artifact: ArtifactId,
    pub operation: OperationId,
    pub intended_commit: CommitId,
    pub declaration_digest: DeclarationDigest,
    pub approval_subject: ApprovalSubject,
}

/// A durable-stream error whose variants are suitable for fail-closed callers.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("the current user's home directory is unavailable")]
    HomeDirectoryUnavailable,
    #[error("durable-state identity contains an unsafe path component: {0}")]
    UnsafeIdentity(String),
    #[error("unable to access durable state at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("unsupported durable stream format version {version} at byte {offset}")]
    UnsupportedFormatVersion { version: u32, offset: u64 },
    #[error("durable stream corruption at byte {offset}: {reason}")]
    CorruptRecord { offset: u64, reason: String },
    #[error("durable stream has an incomplete final record beginning at byte {offset}")]
    TornTail { offset: u64 },
    #[error("intent request does not match this journal's repository and train")]
    IntentDoesNotMatchTrain,
    #[error("train declaration is not pinned in the journal")]
    DeclarationNotPinned,
    #[error("active declaration digest `{active}` differs from pinned digest `{pinned}`")]
    DeclarationDigestMismatch { pinned: String, active: String },
    #[error("declaration binding changed from `{expected}` to `{actual}` before confirmation")]
    DeclarationBindingChanged { expected: String, actual: String },
    #[error("declaration digest `{digest}` is already pinned")]
    DeclarationDigestUnchanged { digest: String },
    #[error("train journal is terminal: {state}")]
    TrainTerminal { state: String },
    #[error("irreversible executor failed: {0}")]
    Executor(SeamError),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Envelope<T> {
    version: u32,
    record: T,
    checksum: String,
}

fn append_record<T: Serialize>(path: &Path, record: T) -> Result<(), StateError> {
    let bytes = encode_record(record)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| state_io(path, source))?;
    file.write_all(&bytes)
        .map_err(|source| state_io(path, source))?;
    file.sync_data().map_err(|source| state_io(path, source))?;
    Ok(())
}

fn encode_record<T: Serialize>(record: T) -> Result<Vec<u8>, StateError> {
    let record_bytes = serde_json::to_vec(&record).map_err(|error| StateError::CorruptRecord {
        offset: 0,
        reason: format!("could not encode durable record: {error}"),
    })?;
    let envelope = Envelope {
        version: STREAM_FORMAT_VERSION,
        record,
        checksum: checksum(&record_bytes),
    };
    let mut bytes = serde_json::to_vec(&envelope).map_err(|error| StateError::CorruptRecord {
        offset: 0,
        reason: format!("could not encode durable envelope: {error}"),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_records<T>(path: &Path) -> Result<Vec<T>, StateError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(state_io(path, source)),
    };
    decode_records(&bytes)
}

fn recover_torn_tail<T>(path: &Path) -> Result<Vec<T>, StateError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(state_io(path, source)),
    };
    match decode_records(&bytes) {
        Ok(records) => Ok(records),
        Err(StateError::TornTail { offset }) => {
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|source| state_io(path, source))?;
            file.set_len(offset)
                .map_err(|source| state_io(path, source))?;
            file.sync_all().map_err(|source| state_io(path, source))?;
            decode_records(&bytes[..offset as usize])
        }
        Err(error) => Err(error),
    }
}

fn decode_records<T>(bytes: &[u8]) -> Result<Vec<T>, StateError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let tail = &bytes[offset..];
        let Some(newline) = tail.iter().position(|byte| *byte == b'\n') else {
            return Err(StateError::TornTail {
                offset: offset as u64,
            });
        };
        let line = &tail[..newline];
        if line.is_empty() {
            return Err(corrupt(offset, "empty records are not permitted"));
        }
        let envelope: Envelope<T> = serde_json::from_slice(line)
            .map_err(|error| corrupt(offset, format!("invalid record framing: {error}")))?;
        if envelope.version != STREAM_FORMAT_VERSION {
            return Err(StateError::UnsupportedFormatVersion {
                version: envelope.version,
                offset: offset as u64,
            });
        }
        let encoded_record = serde_json::to_vec(&envelope.record)
            .map_err(|error| corrupt(offset, format!("could not re-encode record: {error}")))?;
        if envelope.checksum != checksum(&encoded_record) {
            return Err(corrupt(offset, "record checksum does not match"));
        }
        records.push(envelope.record);
        offset += newline + 1;
    }
    Ok(records)
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn corrupt(offset: usize, reason: impl Into<String>) -> StateError {
    StateError::CorruptRecord {
        offset: offset as u64,
        reason: reason.into(),
    }
}

fn state_io(path: &Path, source: io::Error) -> StateError {
    StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn validate_path_component(value: &str) -> Result<(), StateError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(StateError::UnsafeIdentity(value.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn identity() -> TrainJournalIdentity {
        TrainJournalIdentity::new(
            RepositoryId::new("example-repository"),
            TrainId::new("release"),
            "20260830",
        )
        .unwrap()
    }

    fn store() -> (TempDir, JournalStore) {
        let home = tempfile::tempdir().unwrap();
        let store = JournalStore::new(
            home.path().join(".local/share/cortexkit/release"),
            identity(),
        )
        .unwrap();
        (home, store)
    }

    fn approval_subject() -> ApprovalSubject {
        ApprovalSubject {
            repository: RepositoryId::new("example-repository"),
            train: TrainId::new("release"),
            intended_commit: CommitId::new("a1b2c3"),
            declaration_digest: DeclarationDigest::new("declaration-sha256"),
            artifact_digests: Vec::new(),
            public_effects: vec![OperationId::new("publish")],
        }
    }

    fn request() -> EffectRequest {
        EffectRequest {
            repository: RepositoryId::new("example-repository"),
            train: TrainId::new("release"),
            phase: PhaseInstanceId::new("publish-crate"),
            artifact: ArtifactId::new("cortexkit-release"),
            operation: OperationId::new("publish"),
            intended_commit: CommitId::new("a1b2c3"),
            declaration_digest: DeclarationDigest::new("declaration-sha256"),
        }
    }

    #[test]
    fn state_paths_are_outside_the_repository_and_use_train_identity() {
        let (home, store) = store();
        assert_eq!(
            store.journal_path(),
            home.path()
                .join(".local/share/cortexkit/release/example-repository/release-20260830.journal")
        );
        assert_eq!(
            store.intent_path(),
            home.path()
                .join(".local/share/cortexkit/release/example-repository/release-20260830.intent")
        );
        assert!(!store.journal_path().starts_with("."));
    }

    #[test]
    fn future_version_is_refused_before_the_record_is_accepted() {
        let (_home, store) = store();
        let record = JournalRecord::Completion {
            intent: Box::new(pending_intent()),
            evidence: Box::default(),
        };
        let record_bytes = serde_json::to_vec(&record).unwrap();
        let future = Envelope {
            version: STREAM_FORMAT_VERSION + 1,
            record,
            checksum: checksum(&record_bytes),
        };
        fs::write(
            store.journal_path(),
            format!("{}\n", serde_json::to_string(&future).unwrap()),
        )
        .unwrap();

        assert!(matches!(
            store.read_journal(),
            Err(StateError::UnsupportedFormatVersion {
                version: 2,
                offset: 0
            })
        ));
    }

    #[test]
    fn recovery_truncates_only_an_incomplete_final_record() {
        let (_home, store) = store();
        store
            .append_journal(JournalRecord::Completion {
                intent: Box::new(pending_intent()),
                evidence: Box::default(),
            })
            .unwrap();
        let verified_len = fs::metadata(store.journal_path()).unwrap().len();
        let mut bytes = fs::read(store.journal_path()).unwrap();
        bytes.extend_from_slice(b"{\"version\":1,\"record\":");
        fs::write(store.journal_path(), bytes).unwrap();

        assert!(matches!(
            store.read_journal(),
            Err(StateError::TornTail { offset }) if offset == verified_len
        ));
        let recovered = store.recover_torn_journal_tail().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            fs::metadata(store.journal_path()).unwrap().len(),
            verified_len
        );
        assert!(store.read_journal().is_ok());
    }

    #[test]
    fn mid_stream_checksum_corruption_is_refused_and_never_truncated() {
        let (_home, store) = store();
        for _ in 0..2 {
            store
                .append_journal(JournalRecord::Completion {
                    intent: Box::new(pending_intent()),
                    evidence: Box::default(),
                })
                .unwrap();
        }
        let original = fs::read(store.journal_path()).unwrap();
        let mut corrupted = original.clone();
        let index = corrupted
            .windows(b"publish".len())
            .position(|window| window == b"publish")
            .unwrap();
        corrupted[index] = b'x';
        fs::write(store.journal_path(), &corrupted).unwrap();

        assert!(matches!(
            store.recover_torn_journal_tail(),
            Err(StateError::CorruptRecord { offset: 0, .. })
        ));
        assert_eq!(fs::read(store.journal_path()).unwrap(), corrupted);
    }

    #[test]
    fn intent_is_durable_before_the_irreversible_executor_can_run() {
        let (_home, store) = store();
        pin_declaration_for_request(&store);
        assert!(store.pending_intents().unwrap().is_empty());
        let mut executor = IntentCheckingExecutor {
            intent_path: store.intent_path(),
            called: false,
        };

        store
            .execute_with_intent(&request(), approval_subject(), &mut executor)
            .unwrap();

        assert!(executor.called);
        assert_eq!(store.pending_intents().unwrap(), vec![pending_intent()]);
    }

    #[test]
    fn completion_resolves_only_the_matching_pending_intent() {
        let (_home, store) = store();
        pin_declaration_for_request(&store);
        let intent = store.append_intent(&request(), approval_subject()).unwrap();
        store
            .append_completion(&intent, ProbeEvidence::default())
            .unwrap();

        assert!(store.pending_intents().unwrap().is_empty());
        assert_eq!(
            store.read_intents().unwrap(),
            vec![IntentRecord::Pending(intent)]
        );
    }

    #[test]
    fn unsafe_identity_cannot_escape_the_state_root() {
        assert!(matches!(
            TrainJournalIdentity::new(RepositoryId::new("../repo"), TrainId::new("release"), "id"),
            Err(StateError::UnsafeIdentity(_))
        ));
    }

    fn pin_declaration_for_request(store: &JournalStore) {
        store
            .append_journal(JournalRecord::DeclarationPinned {
                binding: DeclarationBinding {
                    digest: request().declaration_digest,
                    normalized: Value::Null,
                },
            })
            .unwrap();
    }

    fn pending_intent() -> PendingIntent {
        PendingIntent {
            sequence: 1,
            train: TrainId::new("release"),
            phase: PhaseInstanceId::new("publish-crate"),
            artifact: ArtifactId::new("cortexkit-release"),
            operation: OperationId::new("publish"),
            intended_commit: CommitId::new("a1b2c3"),
            declaration_digest: DeclarationDigest::new("declaration-sha256"),
            approval_subject: approval_subject(),
        }
    }

    struct IntentCheckingExecutor {
        intent_path: PathBuf,
        called: bool,
    }

    impl IrreversibleExecutor for IntentCheckingExecutor {
        fn execute(&mut self, _request: &EffectRequest) -> Result<ProbeEvidence, SeamError> {
            let bytes =
                fs::read(&self.intent_path).map_err(|error| SeamError::new(error.to_string()))?;
            if !bytes.ends_with(b"\n") || bytes.is_empty() {
                return Err(SeamError::new(
                    "write-ahead intent was not durable before execution",
                ));
            }
            self.called = true;
            Ok(ProbeEvidence::default())
        }
    }
}
