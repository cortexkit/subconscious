//! Durable configuration authority. Callers serialize access under the daemon's
//! gate lock and run disk/keychain work on a blocking worker. A successful commit
//! is the only permission to report approval or start delivery; `anchor_pending`
//! means the bytes committed but their sealed anchor has not caught up yet.
//! While unstamped, retained bytes are history, not authority. In digest-only
//! mode the committed in-memory stamp rules during the run: editing the file's
//! stamped bit cannot re-enable the gate. A restart reads the file as found;
//! stopped-daemon edits and replay are the documented digest-only limitation.

mod disk;
mod writer_lock;
pub use writer_lock::{StoreWriterLock, WriterLockError};
mod records;
#[cfg(feature = "test-support")]
pub mod testing;
#[cfg(test)]
mod tests;

pub use records::*;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
};
use subc_os::config_keychain::{
    AnchorState, KeychainError, KeychainState, KeychainStore, StoreAnchor, StoreKey,
};

pub const STORE_FORMAT_VERSION: u32 = 1;
pub const STORE_FILE: &str = "approved.json";
/// Bound both reads and publications so a successful commit remains readable.
pub const STORE_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const KEYCHAIN_SERVICE: &str = "com.cortexkit.subc.config-gate";

fn wall_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreState {
    Unstamped,
    Stamped,
    StoreUnverified,
    StoreMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorCode {
    StoreUnverified,
    StoreMissing,
    StoreTampered,
    StoreTooLarge,
    AnchorPending,
    GenerationPruned,
    GenerationMissing,
    RevisionExhausted,
    InvalidState,
    Io,
}

#[derive(Debug)]
pub struct StoreError {
    pub code: StoreErrorCode,
    pub detail: String,
}
impl StoreError {
    fn new(code: StoreErrorCode, detail: impl ToString) -> Self {
        Self {
            code,
            detail: detail.to_string(),
        }
    }
    fn serialization(error: impl ToString) -> Self {
        Self::new(StoreErrorCode::InvalidState, error)
    }
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.detail)
    }
}
impl std::error::Error for StoreError {}
impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::new(StoreErrorCode::Io, error)
    }
}

/// Emitted by the store, drained by the caller into the daemon activity log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreActivity {
    pub event: &'static str,
    pub detail: String,
}

/// Shared stand-in anchor for daemon-owner unit tests, never a host keychain.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestStoreKey(std::sync::Arc<std::sync::Mutex<tests::MemoryKey>>);

#[cfg(test)]
impl TestStoreKey {
    pub(crate) fn fail_anchor(&self, fail: bool) {
        self.0.lock().unwrap().set_anchor_failure(fail);
    }

    pub(crate) fn anchor_calls(&self) -> usize {
        self.0.lock().unwrap().anchor_calls()
    }
}

#[derive(Clone, Debug)]
pub struct StoreStatus {
    pub state: StoreState,
    pub integrity: IntegrityMode,
    pub revision: u64,
    pub base_revision: u64,
    pub anchor_pending: bool,
    pub warnings: Vec<String>,
}

/// An absolute path persisted by installation in the service definition. It is
/// never re-resolved from environment variables at store open time.
pub struct StoreRoot(PathBuf);
impl StoreRoot {
    pub fn from_service_definition(path: PathBuf) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "store root must be absolute",
            ));
        }
        Ok(Self(path))
    }
    /// Installation default only. Windows uses the token/registry profile, not
    /// APPDATA, LOCALAPPDATA, USERPROFILE or XDG values from a process.
    pub fn installation_default() -> io::Result<Self> {
        #[cfg(windows)]
        let path = subc_os::profile_dir()?
            .join("AppData")
            .join("Local")
            .join("cortexkit")
            .join("config");
        #[cfg(not(windows))]
        let path = subc_os::real_user_home_dir()?
            .join(".local")
            .join("share")
            .join("cortexkit")
            .join("config");
        Self::from_service_definition(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Envelope {
    format_version: u32,
    revision: u64,
    previous_digest: Option<[u8; 32]>,
    integrity: IntegrityMode,
    body: StoreBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<[u8; 32]>,
}
impl Envelope {
    fn authenticate(&self, key: &StoreKey) -> Result<[u8; 32], StoreError> {
        // Hash the complete envelope without its MAC. This binds the mode and
        // format as well as the body in the crash window before anchoring.
        let mut unsigned = self.clone();
        unsigned.mac = None;
        let body = serde_json::to_vec(&unsigned).map_err(StoreError::serialization)?;
        Ok(self.mac_over_bytes(key, &body))
    }
    fn mac_over_bytes(&self, key: &StoreKey, body: &[u8]) -> [u8; 32] {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts 32 bytes");
        mac.update(b"subc-config-store-v1\0");
        mac.update(&self.revision.to_be_bytes());
        mac.update(&[u8::from(self.previous_digest.is_some())]);
        mac.update(&self.previous_digest.unwrap_or([0; 32]));
        mac.update(&Sha256::digest(body));
        mac.finalize().into_bytes().into()
    }
    fn valid_mac(&self, key: &StoreKey, bytes: &[u8]) -> bool {
        use subtle::ConstantTimeEq;
        let Some(saved) = self.mac else {
            return false;
        };
        // The format writes the MAC as the final field. Authenticate the exact
        // preceding bytes, not a deserialized/reserialized body: unknown fields
        // and changed whitespace must not disappear during crash recovery.
        let mut suffix = b",\"mac\":".to_vec();
        suffix.extend(serde_json::to_vec(&saved).expect("fixed byte array serializes"));
        suffix.push(b'}');
        let Some(prefix) = bytes.strip_suffix(suffix.as_slice()) else {
            return false;
        };
        let mut unsigned = prefix.to_vec();
        unsigned.push(b'}');
        bool::from(saved.ct_eq(&self.mac_over_bytes(key, &unsigned)))
    }
}

enum Backend {
    Unsupported,
    Os(KeychainStore),
    Unavailable(String),
    #[cfg(feature = "test-support")]
    File(subc_os::config_keychain::testing::FileKeyStore),
    #[cfg(test)]
    Memory(std::sync::Arc<std::sync::Mutex<tests::MemoryKey>>),
}
impl Backend {
    fn production() -> Self {
        match KeychainStore::open(KEYCHAIN_SERVICE) {
            Ok(store) => Self::Os(store),
            Err(KeychainError::Unsupported) => Self::Unsupported,
            Err(error) => Self::Unavailable(error.to_string()),
        }
    }
    fn load(&self) -> Result<Option<KeychainState>, StoreError> {
        let result = match self {
            Self::Unsupported => return Ok(None),
            Self::Unavailable(error) => {
                return Err(StoreError::new(StoreErrorCode::StoreUnverified, error))
            }
            Self::Os(store) => store.load(),
            #[cfg(feature = "test-support")]
            Self::File(store) => store.load(),
            #[cfg(test)]
            Self::Memory(store) => return store.lock().unwrap().load(),
        };
        result.map_err(|e| StoreError::new(StoreErrorCode::StoreUnverified, e))
    }
    fn create(&self) -> Result<(), StoreError> {
        let mut bytes = [0; 32];
        getrandom::getrandom(&mut bytes).map_err(StoreError::serialization)?;
        let result = match self {
            Self::Os(store) => store.create(StoreKey::new(bytes)),
            #[cfg(feature = "test-support")]
            Self::File(store) => store.create(StoreKey::new(bytes)),
            #[cfg(test)]
            Self::Memory(store) => {
                store.lock().unwrap().key = Some(bytes);
                return Ok(());
            }
            _ => {
                return Err(StoreError::new(
                    StoreErrorCode::StoreUnverified,
                    "sealing unavailable",
                ))
            }
        };
        result.map_err(|e| StoreError::new(StoreErrorCode::StoreUnverified, e))
    }
    fn seal_available(&self) -> bool {
        !matches!(self, Self::Unsupported)
    }
    fn anchor(&self, pair: StoreAnchor, stamped: bool) -> Result<(), StoreError> {
        let result = match self {
            Self::Os(store) => {
                if stamped {
                    store.update_anchor(pair)
                } else {
                    store.clear_anchor(pair.revision)
                }
            }
            #[cfg(feature = "test-support")]
            Self::File(store) => {
                if stamped {
                    store.update_anchor(pair)
                } else {
                    store.clear_anchor(pair.revision)
                }
            }
            #[cfg(test)]
            Self::Memory(store) => return store.lock().unwrap().anchor(pair, stamped),
            _ => {
                return Err(StoreError::new(
                    StoreErrorCode::StoreUnverified,
                    "key access unavailable; restore key access",
                ))
            }
        };
        result.map_err(|e| StoreError::new(StoreErrorCode::AnchorPending, e))
    }
}

struct UnanchoredAck {
    visible: StoreBody,
    tuple: Tuple,
    generation: u64,
}

#[cfg(test)]
pub(crate) type IoThreads =
    std::sync::Arc<std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>>>;

/// One instance per daemon, serialized by its gate lock. No method reads live
/// configuration files. Diagnostic snapshots are validated and redacted; approved
/// delivery bytes may contain vault reference names, never literal secret values.
pub struct ConfigStore {
    root: StoreRoot,
    backend: Backend,
    envelope: Envelope,
    state: StoreState,
    expected_digest: Option<[u8; 32]>,
    pending_anchor: Option<(StoreAnchor, bool)>,
    pending_prior_anchor: Option<AnchorState>,
    warnings: Vec<String>,
    activities: Vec<StoreActivity>,
    deferred_failure: Option<StoreBody>,
    // The approved-store file containing the ACK has been atomically renamed,
    // but that ACK is not activation authority until the keychain anchor commits.
    // Readers and a competing NACK use the previous pending/active vector.
    unanchored_ack: Option<UnanchoredAck>,
    #[cfg(test)]
    byte_limit: usize,
    #[cfg(test)]
    io_threads: Option<IoThreads>,
    #[cfg(feature = "test-support")]
    hooks: Option<testing::TestSupport>,
}
impl ConfigStore {
    #[cfg(all(test, unix))]
    pub(crate) fn clear_delivery_snapshot_for_test(&mut self, generation: u64) {
        let mut body = self.envelope.body.clone();
        body.generations.get_mut(&generation).unwrap().delivery = None;
        self.publish(body, false, wall_time_ms()).unwrap();
    }
    /// A verified loaded view for gate-locked admission. The owner performs
    /// `check` outside the module gate before preparing the launch witness.
    pub(crate) fn loaded_admission_state(&self) -> (StoreState, &StoreBody) {
        (
            self.state,
            self.deferred_failure
                .as_ref()
                .or(self.unanchored_ack.as_ref().map(|ack| &ack.visible))
                .unwrap_or(&self.envelope.body),
        )
    }

    /// The cache publisher has already verified authority under its owner lock.
    /// Observe the anchor barrier without repeating disk or keychain checks.
    pub(crate) fn loaded_anchor_pending(&self) -> bool {
        self.pending_anchor.is_some()
    }
    /// Recheck a launch witness after a full authority check on a blocking
    /// worker. This is only loaded state, never a replacement for `check`.
    pub(crate) fn loaded_module_generation(&self, module: &str) -> Option<u64> {
        if !matches!(self.state, StoreState::Unstamped | StoreState::Stamped) {
            return None;
        }
        Some(
            self.deferred_failure
                .as_ref()
                .or(self.unanchored_ack.as_ref().map(|ack| &ack.visible))
                .unwrap_or(&self.envelope.body)
                .versions
                .iter()
                .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
                .and_then(|(_, versions)| versions.pending.or(versions.active))
                .unwrap_or(0),
        )
    }
    #[cfg(test)]
    pub(crate) fn observe_io_threads(&mut self, threads: IoThreads) {
        self.io_threads = Some(threads);
    }

    #[cfg(test)]
    fn observe_io_thread(&self, operation: &'static str) {
        if let Some(threads) = &self.io_threads {
            threads
                .lock()
                .unwrap()
                .push((operation, std::thread::current().id()));
        }
    }

    /// Unit tests never consult a host keychain, even on a signed test runner.
    #[cfg(test)]
    pub(crate) fn open_digest_only_for_tests(root: StoreRoot) -> Self {
        Self::load(root, Backend::Unsupported)
    }
    #[cfg(test)]
    pub(crate) fn open_sealed_for_tests(root: StoreRoot, key: TestStoreKey) -> Self {
        Self::load(root, Backend::Memory(key.0))
    }
    /// Reads only; the first stamp creates the directory and file. Caller must
    /// hold its daemon single-writer/start lock until this instance is dropped.
    pub fn open(root: StoreRoot) -> Self {
        Self::load(root, Backend::production())
    }

    fn load(root: StoreRoot, backend: Backend) -> Self {
        let mut store = Self {
            root,
            backend,
            envelope: Envelope {
                format_version: STORE_FORMAT_VERSION,
                revision: 0,
                previous_digest: None,
                integrity: IntegrityMode::DigestOnly,
                body: StoreBody::default(),
                mac: None,
            },
            state: StoreState::Unstamped,
            expected_digest: None,
            pending_anchor: None,
            pending_prior_anchor: None,
            warnings: Vec::new(),
            activities: Vec::new(),
            deferred_failure: None,
            unanchored_ack: None,
            #[cfg(test)]
            byte_limit: STORE_BYTE_LIMIT,
            #[cfg(test)]
            io_threads: None,
            #[cfg(feature = "test-support")]
            hooks: None,
        };
        if let Err(error) = store.start_check() {
            store.state = if error.code == StoreErrorCode::StoreMissing {
                StoreState::StoreMissing
            } else {
                StoreState::StoreUnverified
            };
            store.tamper_activity(&error.to_string());
            store.warnings.push(error.to_string());
        }
        store
    }

    fn start_check(&mut self) -> Result<(), StoreError> {
        let key = self.backend.load()?;
        let bytes = disk::read(&self.path())
            .map_err(|e| StoreError::new(StoreErrorCode::StoreUnverified, e))?;
        self.expected_digest = bytes.as_deref().map(hash);
        // A cleared sealed anchor makes all file states non-authoritative. In
        // particular an old stamped store cannot re-enable the gate after unstamp.
        if key
            .as_ref()
            .is_some_and(|k| k.anchor.current.is_none() && k.anchor.high_water > 0)
        {
            if let Some(bytes) = &bytes {
                if let Ok(envelope) = serde_json::from_slice::<Envelope>(bytes) {
                    if envelope.format_version == STORE_FORMAT_VERSION
                        && !envelope.body.stamped
                        && envelope.revision == key.as_ref().unwrap().anchor.high_water
                        && envelope.valid_mac(&key.as_ref().unwrap().key, bytes)
                    {
                        self.envelope = envelope;
                    }
                }
            }
            self.warnings
                .push("Sealed anchor is cleared; files are authoritative".into());
            if self.envelope.revision == 0 {
                self.warnings.push(
                    "Lost or replayed store: generation-id continuity is not guaranteed".into(),
                );
            }
            self.envelope.integrity = IntegrityMode::Sealed;
            self.envelope.revision = key.as_ref().unwrap().anchor.high_water;
            return Ok(());
        }
        let Some(bytes) = bytes else {
            if key.as_ref().is_some_and(|k| k.anchor.current.is_some()) {
                return Err(StoreError::new(
                    StoreErrorCode::StoreMissing,
                    "anchored store is missing",
                ));
            }
            return Ok(());
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes);
        if value
            .as_ref()
            .ok()
            .and_then(|v| v.get("format_version"))
            .and_then(|v| v.as_u64())
            .is_some_and(|v| v != u64::from(STORE_FORMAT_VERSION))
        {
            return Err(StoreError::new(
                StoreErrorCode::StoreUnverified,
                "unsupported store format",
            ));
        }
        let envelope = match serde_json::from_slice::<Envelope>(&bytes) {
            Ok(envelope) if envelope.revision > 0 => envelope,
            _ if key.as_ref().is_none_or(|k| k.anchor.current.is_none()) => {
                self.tamper_activity("Store unparseable; gate is off");
                self.warnings.push(
                    "Store unparseable; gate is off; generation-id continuity is not guaranteed"
                        .into(),
                );
                return Ok(());
            }
            _ => {
                return Err(StoreError::new(
                    StoreErrorCode::StoreUnverified,
                    "anchored store is unparseable",
                ))
            }
        };
        if let Some(key) = key {
            if let Some(anchor) = key.anchor.current {
                let exact = self.expected_digest == Some(anchor.sha256)
                    && envelope.revision == anchor.revision;
                let next = anchor.revision.checked_add(1) == Some(envelope.revision)
                    && envelope.previous_digest == Some(anchor.sha256)
                    && envelope.valid_mac(&key.key, &bytes);
                if envelope.integrity != IntegrityMode::Sealed || !(exact || next) {
                    return Err(StoreError::new(
                        StoreErrorCode::StoreUnverified,
                        "sealed store does not match anchor",
                    ));
                }
                self.envelope = envelope;
                if next {
                    self.pending_prior_anchor = Some(key.anchor);
                    self.pending_anchor = Some((
                        StoreAnchor {
                            revision: self.envelope.revision,
                            sha256: hash(&bytes),
                        },
                        self.envelope.body.stamped,
                    ));
                    if let Err(error) = self.retry_anchor() {
                        self.warnings.push(error.to_string());
                    }
                }
                self.state = if self.envelope.body.stamped || self.pending_anchor.is_some() {
                    StoreState::Stamped
                } else {
                    StoreState::Unstamped
                };
                return Ok(());
            }
        }
        if envelope.integrity == IntegrityMode::Sealed {
            return Err(StoreError::new(
                StoreErrorCode::StoreUnverified,
                "sealed key missing or unavailable; restore key access",
            ));
        }
        self.state = if envelope.body.stamped {
            StoreState::Stamped
        } else {
            StoreState::Unstamped
        };
        self.envelope = envelope;
        if self.backend.seal_available() {
            self.warnings
                .push("Re-stamp would seal this digest-only store".into());
        }
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.root.0.join(STORE_FILE)
    }
    fn tamper_activity(&mut self, detail: &str) {
        self.activities.push(StoreActivity {
            event: "config.tamper",
            detail: detail.into(),
        });
    }
    fn usable(&self) -> Result<(), StoreError> {
        match self.state {
            StoreState::StoreMissing => Err(StoreError::new(
                StoreErrorCode::StoreMissing,
                "recovery required",
            )),
            StoreState::StoreUnverified => Err(StoreError::new(
                StoreErrorCode::StoreUnverified,
                "recovery required",
            )),
            _ => Ok(()),
        }
    }
    /// Re-check before every authority use, including after delivery writes.
    pub fn check(&mut self) -> Result<(), StoreError> {
        self.usable()?;
        if self.envelope.integrity == IntegrityMode::Sealed {
            #[cfg(test)]
            self.observe_io_thread("keychain_load");
            let valid = self.backend.load().ok().flatten().is_some_and(|key| {
                let current = StoreAnchor {
                    revision: self.envelope.revision,
                    sha256: self.expected_digest.unwrap_or([0; 32]),
                };
                let covered = if self.envelope.body.stamped {
                    key.anchor.current == Some(current)
                } else {
                    key.anchor.current.is_none() && key.anchor.high_water == current.revision
                };
                // Until publication is anchored, only the exact prior anchor
                // or the new committed pair is acceptable. Recovery may replace
                // rejected bytes whose digest does not match that prior anchor.
                let waiting =
                    self.pending_anchor.is_some() && self.pending_prior_anchor == Some(key.anchor);
                covered || waiting
            });
            if !valid {
                self.state = StoreState::StoreUnverified;
                self.tamper_activity(
                    "Sealed anchor changed or key access was lost while daemon was running",
                );
                return Err(StoreError::new(
                    StoreErrorCode::StoreUnverified,
                    "restore sealed key/anchor access",
                ));
            }
        }
        if self.state == StoreState::Unstamped && self.pending_anchor.is_none() {
            return Ok(());
        }
        #[cfg(test)]
        self.observe_io_thread("store_hash");
        let actual = disk::read(&self.path()).map(|bytes| bytes.as_deref().map(hash));
        if !matches!(actual, Ok(digest) if digest == self.expected_digest) {
            self.state = StoreState::StoreUnverified;
            self.tamper_activity("Approved store changed while daemon was running");
            return Err(StoreError::new(
                StoreErrorCode::StoreTampered,
                "approved store changed",
            ));
        }
        Ok(())
    }
    pub fn body(&mut self) -> Result<&StoreBody, StoreError> {
        self.check()?;
        Ok(self
            .deferred_failure
            .as_ref()
            .or(self.unanchored_ack.as_ref().map(|ack| &ack.visible))
            .unwrap_or(&self.envelope.body))
    }
    pub fn status(&mut self) -> StoreStatus {
        let _ = self.check();
        self.loaded_status()
    }

    /// State as of the last integrity check. This is not a fresh integrity read:
    /// it performs no disk or keychain work.
    pub(crate) fn loaded_status(&self) -> StoreStatus {
        StoreStatus {
            state: self.state,
            integrity: self.envelope.integrity,
            revision: self.envelope.revision,
            base_revision: self.envelope.body.base_revision,
            anchor_pending: self.pending_anchor.is_some(),
            warnings: {
                let mut warnings = self.warnings.clone();
                if self.envelope.integrity == IntegrityMode::DigestOnly {
                    warnings.push("Digest-only: stopped-daemon edits, replay, corruption, deletion and data-home relocation are not detected".into());
                }
                warnings.push("Deleting the sealed key and rewriting the store as digest-only bypasses sealing; pre-gate binaries ignore the store".into());
                warnings
            },
        }
    }
    pub fn take_activities(&mut self) -> Vec<StoreActivity> {
        std::mem::take(&mut self.activities)
    }
    pub(crate) fn record_program_refusal(
        &mut self,
        module: &str,
        approved_base: subc_config::Digest,
        refused_identity: subc_config::Digest,
        held_modules: &std::collections::BTreeSet<String>,
    ) -> Result<(), StoreError> {
        self.delivery_ready()?;
        if self.state != StoreState::Stamped {
            return Ok(());
        }
        let refusal = ProgramRefusal {
            module: module.into(),
            approved_base,
            refused_identity,
        };
        if self.envelope.body.program_refusals.contains(&refusal) {
            return Ok(());
        }
        let mut body = self.envelope.body.clone();
        body.program_refusals.push(refusal);
        while body.program_refusals.len() > PROGRAM_REFUSAL_LIMIT {
            if let Some(index) = body
                .program_refusals
                .iter()
                .position(|record| !held_modules.contains(&record.module))
            {
                body.program_refusals.remove(index);
            } else {
                tracing::warn!(
                    retained = body.program_refusals.len(),
                    limit = PROGRAM_REFUSAL_LIMIT,
                    "program refusal limit exceeded: every remaining record protects a held module"
                );
                break;
            }
        }
        let result = self.publish(body, false, wall_time_ms());
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|error| error.code == StoreErrorCode::AnchorPending)
        {
            self.activities.push(StoreActivity {
                event: "config.refused",
                detail: format!("{module}: program_refused; covered identity is not approved"),
            });
        }
        result
    }
    /// No approval/delivery may rely on a pending revision before this succeeds.
    pub fn delivery_ready(&mut self) -> Result<(), StoreError> {
        self.check()?;
        if self.pending_anchor.is_some() {
            return Err(StoreError::new(
                StoreErrorCode::AnchorPending,
                "anchor update pending",
            ));
        }
        Ok(())
    }
    pub fn retry_anchor(&mut self) -> Result<(), StoreError> {
        self.check()?;
        if let Some((pair, stamped)) = self.pending_anchor {
            self.backend.anchor(pair, stamped)?;
            self.pending_anchor = None;
            self.pending_prior_anchor = None;
            if let Some(ack) = self.unanchored_ack.take() {
                // A different module may fail while this ACK is unanchored.
                // Preserve that failure without discarding the now-anchored ACK.
                if let Some(failure) = self.deferred_failure.as_mut() {
                    let still_pending = failure.versions.iter().any(|(tuple, versions)| {
                        tuple == &ack.tuple && versions.pending == Some(ack.generation)
                    });
                    let failed_module =
                        failure
                            .generations
                            .get(&ack.generation)
                            .is_some_and(|entry| {
                                entry.generation.never_redeliver.contains(&ack.tuple.module)
                            });
                    if still_pending && !failed_module {
                        if let Some((_, approved)) = self
                            .envelope
                            .body
                            .versions
                            .iter()
                            .find(|(tuple, _)| tuple == &ack.tuple)
                        {
                            if let Some((_, visible)) = failure
                                .versions
                                .iter_mut()
                                .find(|(tuple, _)| tuple == &ack.tuple)
                            {
                                *visible = approved.clone();
                            }
                        }
                        if let Some(entry) = failure.generations.get_mut(&ack.generation) {
                            entry
                                .generation
                                .module_states
                                .insert(ack.tuple.module, subc_config::GenerationState::Active);
                            if !entry
                                .generation
                                .module_states
                                .values()
                                .any(|state| *state == subc_config::GenerationState::Failed)
                            {
                                entry.outcome = "active".into();
                            }
                        }
                    }
                }
            }
            self.state = if stamped {
                StoreState::Stamped
            } else {
                StoreState::Unstamped
            };
        }
        if let Some(body) = self.deferred_failure.take() {
            if let Err(error) = self.publish(body.clone(), false, wall_time_ms()) {
                self.deferred_failure = Some(body);
                return Err(error);
            }
        }
        Ok(())
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl ConfigStore {
    fn next_revision(&self, sealed: bool, fresh: bool) -> Result<u64, StoreError> {
        let previous = if sealed {
            self.backend
                .load()?
                .ok_or_else(|| {
                    StoreError::new(StoreErrorCode::StoreUnverified, "sealing key missing")
                })?
                .anchor
                .high_water
        } else if fresh {
            // A rejected sealed counter is never trusted. Digest-only expressly
            // has no replay protection, including when recovering a parsed file.
            disk::read(&self.path())
                .ok()
                .flatten()
                .and_then(|b| serde_json::from_slice::<Envelope>(&b).ok())
                .map_or(0, |e| {
                    if e.revision == u64::MAX {
                        0
                    } else {
                        e.revision
                    }
                })
        } else {
            self.envelope.revision
        };
        previous.checked_add(1).ok_or_else(|| {
            StoreError::new(
                StoreErrorCode::RevisionExhausted,
                "store revision exhausted",
            )
        })
    }

    fn publish(&mut self, body: StoreBody, fresh: bool, now_ms: u64) -> Result<(), StoreError> {
        if self.pending_anchor.is_some() {
            return Err(StoreError::new(
                StoreErrorCode::AnchorPending,
                "anchor update pending",
            ));
        }
        if !fresh {
            self.check()?;
        } else {
            self.expected_digest = disk::read(&self.path())
                .map_err(|e| StoreError::new(StoreErrorCode::StoreUnverified, e))?
                .as_deref()
                .map(hash);
        }
        let sealed = self.envelope.integrity == IntegrityMode::Sealed;
        let revision = self.next_revision(sealed, fresh)?;
        let mut envelope = Envelope {
            format_version: STORE_FORMAT_VERSION,
            revision,
            previous_digest: self.expected_digest,
            integrity: self.envelope.integrity,
            body,
            mac: None,
        };
        let mut prior_anchor = None;
        let key = if sealed {
            let key = self
                .backend
                .load()?
                .ok_or_else(|| StoreError::new(StoreErrorCode::StoreUnverified, "key missing"))?;
            prior_anchor = Some(key.anchor);
            Some(key)
        } else {
            None
        };
        #[cfg(test)]
        let limit = self.byte_limit;
        #[cfg(not(test))]
        let limit = STORE_BYTE_LIMIT;
        let bytes = loop {
            envelope.mac = key
                .as_ref()
                .map(|key| envelope.authenticate(&key.key))
                .transpose()?;
            let bytes = serde_json::to_vec(&envelope).map_err(StoreError::serialization)?;
            if bytes.len() <= limit {
                break bytes;
            }
            if !envelope.body.prune_oldest(now_ms) {
                return Err(StoreError::new(StoreErrorCode::StoreTooLarge,
                    format!("candidate exceeds STORE_BYTE_LIMIT ({limit} bytes); active, recovery and pending versions cannot be pruned")));
            }
        };
        if let Err(error) = disk::publish(&self.path(), &bytes, |point| {
            #[cfg(feature = "test-support")]
            if let Some(hooks) = &self.hooks {
                hooks.pause(point)?;
            }
            let _ = point;
            Ok(())
        }) {
            // A failure after rename may already have exposed new bytes. Do not
            // report success or pretend the old in-memory authority is durable.
            self.state = StoreState::StoreUnverified;
            return Err(error.into());
        }
        self.expected_digest = Some(hash(&bytes));
        self.state = if sealed || envelope.body.stamped {
            StoreState::Stamped
        } else {
            StoreState::Unstamped
        };
        self.envelope = envelope;
        if sealed {
            self.pending_prior_anchor = prior_anchor;
            self.pending_anchor = Some((
                StoreAnchor {
                    revision,
                    sha256: hash(&bytes),
                },
                self.envelope.body.stamped,
            ));
            #[cfg(feature = "test-support")]
            if let Some(hooks) = &self.hooks {
                hooks.pause(disk::PausePoint::BeforeAnchor)?;
            }
            self.retry_anchor()?;
        }
        Ok(())
    }

    /// Receives a fully approved generation. `observed` must be a fresh snapshot
    /// of all files, not only the plan's read set, so unrelated edits survive
    /// proposal rebasing. The caller has checked plan freshness and approval.
    pub fn commit_generation(
        &mut self,
        mut entry: HistoryEntry,
        approved: Snapshot,
        observed: Snapshot,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        self.delivery_ready()?;
        let kind = entry.kind;
        if entry.generation.approval_channel == subc_config::ApprovalChannel::DevelopmentPlacement
            && kind != CommitKind::Development
        {
            return Err(StoreError::new(
                StoreErrorCode::InvalidState,
                "development placement is not an approval channel for this commit",
            ));
        }
        if self.state == StoreState::Unstamped && kind != CommitKind::Stamp {
            return Err(StoreError::new(
                StoreErrorCode::InvalidState,
                "first commit must be a stamp",
            ));
        }
        if kind == CommitKind::Stamp {
            self.select_stamp_mode()?;
        }
        let id = self
            .envelope
            .body
            .last_generation
            .checked_add(1)
            .ok_or_else(|| {
                StoreError::new(
                    StoreErrorCode::RevisionExhausted,
                    "generation ids exhausted",
                )
            })?;
        entry.generation.id = id;
        let mut body = self.envelope.body.clone();
        body.last_generation = id;
        body.base_revision = self.next_revision(
            self.envelope.integrity == IntegrityMode::Sealed,
            kind == CommitKind::Stamp,
        )?;
        body.stamped = kind != CommitKind::Unstamp;
        // Development generations describe program placement, not a new active
        // configuration vector. Their tuple pointers remain untouched.
        if kind != CommitKind::Development {
            body.approved = approved;
            if kind == CommitKind::Unstamp {
                body.program_refusals.clear();
            } else if !body.program_refusals.is_empty() {
                let snapshot = crate::config_gate::GateSnapshot::from_store(&body.approved)
                    .map_err(StoreError::serialization)?;
                body.program_refusals.retain(|refusal| {
                    crate::config_gate::program::approved_base(&snapshot, &refusal.module)
                        == refusal.approved_base
                });
            }
            for tuple in &entry.tuples {
                let index = body
                    .versions
                    .iter()
                    .position(|(t, _)| t == tuple)
                    .unwrap_or_else(|| {
                        body.versions.push((tuple.clone(), Versions::default()));
                        body.versions.len() - 1
                    });
                body.versions[index].1.pending = Some(id);
            }
        }
        body.generations.insert(id, entry);
        body.observed = if kind == CommitKind::Unstamp {
            None
        } else {
            Some(observed)
        };
        if kind == CommitKind::Stamp {
            body.proposal = None;
        }
        let proposal_changed = body.rebase_proposal()?;
        body.prune(now_ms);
        self.publish(body, kind == CommitKind::Stamp, now_ms)?;
        if proposal_changed {
            self.proposal_activity();
        }
        Ok(id)
    }

    fn select_stamp_mode(&mut self) -> Result<(), StoreError> {
        if self.backend.seal_available() {
            if self.backend.load()?.is_none() {
                self.backend.create()?;
            }
            self.envelope.integrity = IntegrityMode::Sealed;
        } else {
            self.envelope.integrity = IntegrityMode::DigestOnly;
        }
        Ok(())
    }

    /// Approved recovery exits discard every record from the rejected store.
    /// Only a present readable keychain item's high-water survives. The caller
    /// supplies a newly validated file snapshot and approval, just as for stamp.
    pub fn recovery_exit(
        &mut self,
        mut entry: HistoryEntry,
        snapshot: Snapshot,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        if !matches!(
            self.state,
            StoreState::StoreUnverified | StoreState::StoreMissing
        ) || !matches!(entry.kind, CommitKind::Stamp | CommitKind::Unstamp)
        {
            return Err(StoreError::new(
                StoreErrorCode::InvalidState,
                "not a recovery exit",
            ));
        }
        self.select_stamp_mode()?;
        let revision =
            self.next_revision(self.envelope.integrity == IntegrityMode::Sealed, true)?;
        entry.generation.id = 1;
        let stamped = entry.kind == CommitKind::Stamp;
        let mut body = StoreBody {
            stamped,
            base_revision: revision,
            last_generation: 1,
            approved: snapshot,
            ..StoreBody::default()
        };
        for tuple in &entry.tuples {
            body.versions.push((
                tuple.clone(),
                Versions {
                    pending: Some(1),
                    ..Versions::default()
                },
            ));
        }
        body.generations.insert(1, entry);
        body.prune(now_ms);
        self.pending_anchor = None;
        self.pending_prior_anchor = None;
        self.deferred_failure = None;
        self.unanchored_ack = None;
        self.publish(body, true, now_ms)?;
        self.warnings
            .push("Recovery reset history and generation ids; continuity is not guaranteed".into());
        Ok(1)
    }

    fn proposal_activity(&mut self) {
        self.activities.push(StoreActivity {
            event: "config.proposal",
            detail: "Pending configuration diff changed".into(),
        });
    }
    /// Before stamp this is informational and never creates a store file.
    pub fn observe(&mut self, snapshot: Snapshot) -> Result<bool, StoreError> {
        self.delivery_ready()?;
        let mut body = self.envelope.body.clone();
        body.observed = Some(snapshot);
        let changed = body.rebase_proposal()?;
        let identity_changed = body.proposal != self.envelope.body.proposal;
        if self.state == StoreState::Stamped && identity_changed {
            self.publish(body, false, wall_time_ms())?;
        } else if self.state == StoreState::Unstamped {
            self.envelope.body = body;
        }
        if changed {
            self.proposal_activity();
        }
        Ok(changed)
    }

    /// ACK/NACK for a superseded generation changes nothing. A failure restores
    /// the active pointer immediately, even if its durable mark must wait for an
    /// anchor retry. ACKs never change memory while an anchor is pending.
    ///
    /// Nothing is delivered while an earlier anchor is pending. Once repaired,
    /// an unanchored NACKed generation may return at most once per daemon start,
    /// never within one run and never after its failure is anchored. The crash
    /// window and persistent-keychain-failure proof is
    /// `config_delivery_anchor_pending_restart_before_failure_anchor_may_redeliver_once`.
    pub fn record_delivery(
        &mut self,
        tuple: &Tuple,
        generation: u64,
        ack: bool,
    ) -> Result<(), StoreError> {
        self.check()?;
        let mut body = self
            .deferred_failure
            .as_ref()
            .or(self.unanchored_ack.as_ref().map(|ack| &ack.visible))
            .unwrap_or(&self.envelope.body)
            .clone();
        let before_ack = ack.then(|| body.clone());
        let Some((_, versions)) = body.versions.iter_mut().find(|(t, _)| t == tuple) else {
            return Ok(());
        };
        if versions.pending != Some(generation) {
            return Ok(());
        }
        if self.pending_anchor.is_some() && ack {
            return Err(StoreError::new(
                StoreErrorCode::AnchorPending,
                "ACK not recorded",
            ));
        }
        versions.pending = None;
        if ack {
            versions.recovery = versions.active;
            versions.active = Some(generation);
        }
        let entry = body
            .generations
            .get_mut(&generation)
            .ok_or_else(|| StoreError::new(StoreErrorCode::GenerationMissing, generation))?;
        entry.generation.module_states.insert(
            tuple.module.clone(),
            if ack {
                subc_config::GenerationState::Active
            } else {
                subc_config::GenerationState::Failed
            },
        );
        if !ack && !entry.generation.never_redeliver.contains(&tuple.module) {
            entry.generation.never_redeliver.push(tuple.module.clone());
        }
        entry.outcome = if ack { "active" } else { "failed" }.into();
        if self.pending_anchor.is_some() {
            self.deferred_failure = Some(body);
            if !self.warnings.iter().any(|w| w.contains("redeliver")) {
                self.warnings.push(
                    "Failure held in memory while anchor is pending; restart may redeliver once"
                        .into(),
                );
            }
            return Err(StoreError::new(
                StoreErrorCode::AnchorPending,
                "failure restored in memory; durable mark pending",
            ));
        }
        let result = self.publish(body, false, wall_time_ms());
        if ack && result.is_err() && self.pending_anchor.is_some() {
            self.unanchored_ack = before_ack.map(|visible| UnanchoredAck {
                visible,
                tuple: tuple.clone(),
                generation,
            });
        }
        result
    }

    pub fn rollback_configuration(&mut self, generation: u64) -> Result<Snapshot, StoreError> {
        let body = self.body()?;
        if body.pruned.contains_key(&generation) {
            return Err(StoreError::new(
                StoreErrorCode::GenerationPruned,
                generation,
            ));
        }
        body.generations
            .get(&generation)
            .map(|e| e.configuration.clone())
            .ok_or_else(|| StoreError::new(StoreErrorCode::GenerationMissing, generation))
    }
}

impl ConfigStore {
    /// Persist a resumed attempt before notifying a module after daemon restart.
    /// Never-redeliver and superseded targets cannot be resumed.
    pub fn resume_delivery(&mut self, tuple: &Tuple, generation: u64) -> Result<bool, StoreError> {
        self.delivery_ready()?;
        let mut body = self.envelope.body.clone();
        if !body
            .versions
            .iter()
            .any(|(t, v)| t == tuple && v.pending == Some(generation))
        {
            return Ok(false);
        }
        let entry = body
            .generations
            .get_mut(&generation)
            .ok_or_else(|| StoreError::new(StoreErrorCode::GenerationMissing, generation))?;
        if entry.generation.never_redeliver.contains(&tuple.module) {
            return Ok(false);
        }
        entry.outcome = "resumed".into();
        self.publish(body, false, wall_time_ms())?;
        Ok(true)
    }
}
