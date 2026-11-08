use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use subc_config::{raw_digest, Changeset, Generation, InputVector, RecordedValue};

#[derive(Default)]
pub(super) struct MemoryKey {
    pub key: Option<[u8; 32]>,
    anchor: subc_os::config_keychain::AnchorState,
    fail: bool,
    denied: bool,
    anchor_calls: usize,
}
impl MemoryKey {
    pub(super) fn set_anchor_failure(&mut self, fail: bool) {
        self.fail = fail;
    }
    pub(super) fn anchor_calls(&self) -> usize {
        self.anchor_calls
    }
    pub fn load(&self) -> Result<Option<KeychainState>, StoreError> {
        if self.denied {
            return Err(StoreError::new(
                StoreErrorCode::StoreUnverified,
                "restore key access",
            ));
        }
        Ok(self.key.map(|key| KeychainState {
            key: StoreKey::new(key),
            anchor: self.anchor,
        }))
    }
    pub fn anchor(&mut self, pair: StoreAnchor, stamped: bool) -> Result<(), StoreError> {
        self.anchor_calls += 1;
        if self.fail {
            return Err(StoreError::new(
                StoreErrorCode::AnchorPending,
                "injected anchor failure",
            ));
        }
        if stamped && self.anchor.current == Some(pair) {
            return Ok(());
        }
        if !stamped && self.anchor.current.is_none() && self.anchor.high_water == pair.revision {
            return Ok(());
        }
        assert!(pair.revision > self.anchor.high_water);
        self.anchor = subc_os::config_keychain::AnchorState {
            current: stamped.then_some(pair),
            high_water: pair.revision,
        };
        Ok(())
    }
}
struct Rig {
    path: PathBuf,
    key: Arc<Mutex<MemoryKey>>,
}
impl Rig {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "subc-config-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self {
            path,
            key: Arc::new(Mutex::new(MemoryKey::default())),
        }
    }
    fn open(&self, sealed: bool) -> ConfigStore {
        ConfigStore::load(
            StoreRoot(self.path.clone()),
            if sealed {
                Backend::Memory(self.key.clone())
            } else {
                Backend::Unsupported
            },
        )
    }
    fn bytes(&self) -> Vec<u8> {
        fs::read(self.path.join(STORE_FILE)).unwrap()
    }
    fn replace(&self, bytes: &[u8]) {
        fs::write(self.path.join(STORE_FILE), bytes).unwrap();
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn snapshot(a: i32, b: i32) -> Snapshot {
    [
        ("layer-a".into(), RecordedValue::new(serde_json::json!(a))),
        ("layer-b".into(), RecordedValue::new(serde_json::json!(b))),
    ]
    .into()
}
fn tuple() -> Tuple {
    Tuple {
        module: "sample".into(),
        project: Some("project".into()),
    }
}
fn entry(kind: CommitKind) -> HistoryEntry {
    HistoryEntry {
        delivery: None,
        generation: Generation::new(
            0,
            raw_digest(b"approved-plan"),
            InputVector::default(),
            Changeset::new(),
        ),
        kind,
        approval_id: "approval".into(),
        outcome: "pending".into(),
        tuples: BTreeSet::from([tuple()]),
        configuration: snapshot(1, 1),
    }
}
fn commit(store: &mut ConfigStore, kind: CommitKind) -> u64 {
    store
        .commit_generation(entry(kind), snapshot(1, 1), snapshot(1, 1), 12)
        .unwrap()
}
fn error_code<T: std::fmt::Debug>(result: Result<T, StoreError>, code: StoreErrorCode) {
    assert_eq!(result.unwrap_err().code, code);
}

#[test]
fn config_store_digest_commit_revisions_delivery_and_restart() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    store.observe(snapshot(2, 1)).unwrap();
    assert!(!rig.path.join(STORE_FILE).exists());
    assert_eq!(commit(&mut store, CommitKind::Stamp), 1);
    let first = store.status();
    assert_eq!(first.revision, 1);
    assert_eq!(first.base_revision, 1);
    assert!(store.body().unwrap().proposal.is_none());
    store.observe(snapshot(2, 1)).unwrap();
    assert_eq!(store.status().revision, 2);
    assert_eq!(store.status().base_revision, 1);
    store.record_delivery(&tuple(), 1, true).unwrap();
    assert_eq!(store.status().revision, 3);
    assert_eq!(store.status().base_revision, 1);
    let next = commit(&mut store, CommitKind::Approved);
    let mut resumed = rig.open(false);
    assert_eq!(resumed.body().unwrap().versions[0].1.pending, Some(next));
    resumed.record_delivery(&tuple(), next, false).unwrap();
    let mut restarted = rig.open(false);
    assert_eq!(restarted.body().unwrap().versions[0].1.active, Some(1));
    assert_eq!(restarted.body().unwrap().versions[0].1.pending, None);
    assert!(restarted.body().unwrap().generations[&next]
        .generation
        .never_redeliver
        .contains(&"sample".into()));
    let before = restarted.status().revision;
    restarted.record_delivery(&tuple(), 1, false).unwrap();
    assert_eq!(restarted.status().revision, before);
}

#[test]
fn config_store_sealed_crash_catchup_mac_binds_body_and_replay_refused() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    let anchored = rig.bytes();
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            2,
        ),
        StoreErrorCode::AnchorPending,
    );
    let unanchored = rig.bytes();
    rig.key.lock().unwrap().fail = false;
    let mut restarted = rig.open(true);
    assert_eq!(restarted.status().state, StoreState::Stamped);
    assert_eq!(rig.key.lock().unwrap().anchor.high_water, 2);
    rig.replace(&anchored);
    assert_eq!(rig.open(true).status().state, StoreState::StoreUnverified);
    // Recreate the anchor+1 window and change a body byte without its MAC.
    rig.key.lock().unwrap().anchor = subc_os::config_keychain::AnchorState {
        current: Some(StoreAnchor {
            revision: 1,
            sha256: hash(&anchored),
        }),
        high_water: 1,
    };
    let mut tampered: Envelope = serde_json::from_slice(&unanchored).unwrap();
    tampered.body.last_generation = 44;
    rig.replace(&serde_json::to_vec(&tampered).unwrap());
    assert_eq!(rig.open(true).status().state, StoreState::StoreUnverified);
}

#[test]
fn config_store_unreported_alternate_branch_cannot_replay_after_reported_commit() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    let anchored = rig.bytes();
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            2,
        ),
        StoreErrorCode::AnchorPending,
    );
    let alternate = rig.bytes();
    rig.replace(&anchored);
    rig.key.lock().unwrap().fail = false;
    let mut restarted = rig.open(true);
    restarted
        .commit_generation(
            entry(CommitKind::Approved),
            snapshot(3, 1),
            snapshot(3, 1),
            2,
        )
        .unwrap();
    rig.replace(&alternate);
    assert_eq!(rig.open(true).status().state, StoreState::StoreUnverified);
}

#[test]
fn config_store_anchor_pending_blocks_writes_and_ack_but_restores_failure_in_memory() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), 1, true).unwrap();
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            2,
        ),
        StoreErrorCode::AnchorPending,
    );
    let bytes = rig.bytes();
    error_code(store.delivery_ready(), StoreErrorCode::AnchorPending);
    error_code(store.observe(snapshot(3, 1)), StoreErrorCode::AnchorPending);
    error_code(
        store.record_delivery(&tuple(), 2, true),
        StoreErrorCode::AnchorPending,
    );
    assert_eq!(store.body().unwrap().versions[0].1.pending, Some(2));
    error_code(
        store.record_delivery(&tuple(), 2, false),
        StoreErrorCode::AnchorPending,
    );
    assert_eq!(store.body().unwrap().versions[0].1.active, Some(1));
    assert_eq!(store.body().unwrap().versions[0].1.pending, None);
    assert_eq!(rig.bytes(), bytes);
    assert!(store.status().anchor_pending);
    rig.key.lock().unwrap().fail = false;
    store.retry_anchor().unwrap();
    store.delivery_ready().unwrap();
    let mut restarted = rig.open(true);
    assert_eq!(restarted.body().unwrap().versions[0].1.pending, None);
    assert!(restarted.body().unwrap().generations[&2]
        .generation
        .never_redeliver
        .contains(&"sample".into()));
    // The failure is a separate anchored write, beyond the generation commit.
    assert_eq!(rig.key.lock().unwrap().anchor.high_water, 4);
}

fn ack_publication_pending() -> (Rig, ConfigStore) {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), 1, true).unwrap();
    commit(&mut store, CommitKind::Approved);
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.record_delivery(&tuple(), 2, true),
        StoreErrorCode::AnchorPending,
    );
    assert!(store.status().anchor_pending);
    (rig, store)
}

#[test]
fn config_store_ack_publication_anchor_failure_keeps_previous_active_until_retry() {
    let (rig, mut store) = ack_publication_pending();
    let versions = &store.body().unwrap().versions[0].1;
    assert_eq!(
        versions.active,
        Some(1),
        "an unanchored ACK must not activate generation 2"
    );
    assert_eq!(versions.recovery, None);
    assert_eq!(versions.pending, Some(2));
    rig.key.lock().unwrap().fail = false;
    store.retry_anchor().unwrap();
    let versions = &store.body().unwrap().versions[0].1;
    assert_eq!(
        (versions.active, versions.recovery, versions.pending),
        (Some(2), Some(1), None)
    );
    assert!(!store.status().anchor_pending);
    let mut restarted = rig.open(true);
    assert_eq!(restarted.body().unwrap().versions[0].1.active, Some(2));
}

#[test]
fn config_store_ack_publication_anchor_failure_nack_overrides_the_unanchored_ack() {
    let (rig, mut store) = ack_publication_pending();
    error_code(
        store.record_delivery(&tuple(), 2, false),
        StoreErrorCode::AnchorPending,
    );
    let body = store.body().unwrap();
    let versions = &body.versions[0].1;
    assert_eq!(
        (versions.active, versions.recovery, versions.pending),
        (Some(1), None, None)
    );
    assert_eq!(
        body.generations[&2].generation.never_redeliver,
        vec!["sample"]
    );
    rig.key.lock().unwrap().fail = false;
    store.retry_anchor().unwrap();
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    assert_eq!(body.versions[0].1.active, Some(1));
    assert_eq!(body.versions[0].1.pending, None);
    assert_eq!(
        body.generations[&2].generation.never_redeliver,
        vec!["sample"]
    );
}

#[test]
fn config_store_ack_publication_anchor_failure_other_module_nack_does_not_undo_ack() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    let other = Tuple {
        module: "another".into(),
        project: None,
    };
    let mut first = entry(CommitKind::Stamp);
    first.tuples.insert(other.clone());
    store
        .commit_generation(first, snapshot(1, 1), snapshot(1, 1), 1)
        .unwrap();
    store.record_delivery(&tuple(), 1, true).unwrap();
    store.record_delivery(&other, 1, true).unwrap();
    let mut second = entry(CommitKind::Approved);
    second.tuples.insert(other.clone());
    store
        .commit_generation(second, snapshot(2, 1), snapshot(2, 1), 2)
        .unwrap();
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.record_delivery(&tuple(), 2, true),
        StoreErrorCode::AnchorPending,
    );
    error_code(
        store.record_delivery(&other, 2, false),
        StoreErrorCode::AnchorPending,
    );
    assert_eq!(
        store
            .body()
            .unwrap()
            .versions
            .iter()
            .find(|(key, _)| key == &tuple())
            .unwrap()
            .1
            .active,
        Some(1)
    );
    rig.key.lock().unwrap().fail = false;
    store.retry_anchor().unwrap();
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    let acked = &body
        .versions
        .iter()
        .find(|(key, _)| key == &tuple())
        .unwrap()
        .1;
    let nacked = &body
        .versions
        .iter()
        .find(|(key, _)| key == &other)
        .unwrap()
        .1;
    assert_eq!(
        (acked.active, acked.recovery, acked.pending),
        (Some(2), Some(1), None)
    );
    assert_eq!(
        (nacked.active, nacked.recovery, nacked.pending),
        (Some(1), None, None)
    );
    assert_eq!(
        body.generations[&2].generation.never_redeliver,
        vec!["another"]
    );
}

#[test]
fn config_store_tamper_check_after_delivery_write_every_mode() {
    for sealed in [false, true] {
        let rig = Rig::new();
        let mut store = rig.open(sealed);
        commit(&mut store, CommitKind::Stamp);
        store.record_delivery(&tuple(), 1, true).unwrap();
        rig.replace(b"changed");
        error_code(store.body(), StoreErrorCode::StoreTampered);
        assert_eq!(store.status().state, StoreState::StoreUnverified);
        assert_eq!(
            store
                .take_activities()
                .iter()
                .filter(|a| a.event == "config.tamper")
                .count(),
            1
        );
    }
}

#[test]
fn config_store_start_format_corruption_missing_key_denial_and_modes() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    commit(&mut store, CommitKind::Stamp);
    let mut value: serde_json::Value = serde_json::from_slice(&rig.bytes()).unwrap();
    value["format_version"] = serde_json::json!(STORE_FORMAT_VERSION + 1);
    rig.replace(&serde_json::to_vec(&value).unwrap());
    assert_eq!(rig.open(false).status().state, StoreState::StoreUnverified);
    rig.replace(b"truncated");
    let mut corrupted = rig.open(false);
    assert_eq!(corrupted.status().state, StoreState::Unstamped);
    assert_eq!(corrupted.take_activities()[0].event, "config.tamper");
    fs::remove_file(rig.path.join(STORE_FILE)).unwrap();
    assert_eq!(rig.open(false).status().state, StoreState::Unstamped);
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    assert_eq!(rig.open(false).status().state, StoreState::StoreUnverified);
    rig.key.lock().unwrap().denied = true;
    let mut denied = rig.open(true);
    assert_eq!(denied.status().state, StoreState::StoreUnverified);
    let before = rig.bytes();
    error_code(
        denied.recovery_exit(entry(CommitKind::Unstamp), snapshot(1, 1), 1),
        StoreErrorCode::StoreUnverified,
    );
    assert_eq!(before, rig.bytes());
    rig.key.lock().unwrap().denied = false;
    fs::remove_file(rig.path.join(STORE_FILE)).unwrap();
    assert_eq!(rig.open(true).status().state, StoreState::StoreMissing);
}

#[test]
fn config_store_unstamp_restamp_high_water_and_generation_continuity() {
    for sealed in [false, true] {
        let rig = Rig::new();
        let mut store = rig.open(sealed);
        commit(&mut store, CommitKind::Stamp);
        let stamped = rig.bytes();
        commit(&mut store, CommitKind::Unstamp);
        let mut restarted = rig.open(sealed);
        assert_eq!(restarted.status().state, StoreState::Unstamped);
        assert_eq!(commit(&mut restarted, CommitKind::Stamp), 3);
        assert_eq!(restarted.status().revision, 3);
        if sealed {
            commit(&mut restarted, CommitKind::Unstamp);
            rig.replace(&stamped);
            let mut replayed = rig.open(true);
            assert_eq!(replayed.status().state, StoreState::Unstamped);
            commit(&mut replayed, CommitKind::Stamp);
            assert_eq!(replayed.status().revision, 5);
        }
    }
}

#[test]
fn config_store_recovery_uses_anchor_high_water_not_rejected_counter() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    let mut tampered: Envelope = serde_json::from_slice(&rig.bytes()).unwrap();
    tampered.revision = u64::MAX;
    rig.replace(&serde_json::to_vec(&tampered).unwrap());
    let mut recovery = rig.open(true);
    assert_eq!(recovery.status().state, StoreState::StoreUnverified);
    recovery
        .recovery_exit(entry(CommitKind::Stamp), snapshot(9, 9), 9)
        .unwrap();
    assert_eq!(recovery.status().revision, 2);
    assert_eq!(recovery.body().unwrap().generations.len(), 1);
    // A deleted key is recreated with a fresh secret at revision 1.
    rig.key.lock().unwrap().key = None;
    rig.key.lock().unwrap().anchor = Default::default();
    let mut missing_key = rig.open(true);
    assert_eq!(missing_key.status().state, StoreState::StoreUnverified);
    missing_key
        .recovery_exit(entry(CommitKind::Unstamp), snapshot(1, 1), 3)
        .unwrap();
    assert_eq!(missing_key.status().revision, 1);
    assert_eq!(rig.key.lock().unwrap().anchor.high_water, 1);
    assert!(rig.key.lock().unwrap().anchor.current.is_none());
}

#[test]
fn config_store_digest_recovery_counter_rules_and_exhaustion() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    commit(&mut store, CommitKind::Stamp);
    let mut bad: Envelope = serde_json::from_slice(&rig.bytes()).unwrap();
    bad.format_version += 1;
    bad.revision = 41;
    rig.replace(&serde_json::to_vec(&bad).unwrap());
    let mut recovery = rig.open(false);
    recovery
        .recovery_exit(entry(CommitKind::Stamp), snapshot(1, 1), 1)
        .unwrap();
    assert_eq!(recovery.status().revision, 42);
    bad.revision = u64::MAX;
    rig.replace(&serde_json::to_vec(&bad).unwrap());
    let mut recovery = rig.open(false);
    recovery
        .recovery_exit(entry(CommitKind::Stamp), snapshot(1, 1), 1)
        .unwrap();
    assert_eq!(recovery.status().revision, 1);
}

#[test]
fn config_store_proposal_identity_dedup_rebase_and_base_revision() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    commit(&mut store, CommitKind::Stamp);
    assert!(store.observe(snapshot(2, 3)).unwrap());
    let proposal = store.body().unwrap().proposal.clone().unwrap();
    let revision = store.status().revision;
    assert!(!store.observe(snapshot(2, 3)).unwrap());
    assert_eq!(store.status().revision, revision);
    assert_eq!(store.body().unwrap().proposal.as_ref().unwrap(), &proposal);
    store.take_activities();
    store
        .commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 3),
            1,
        )
        .unwrap();
    let rebased = store.body().unwrap().proposal.clone().unwrap();
    assert_eq!(rebased.diff.len(), 1);
    assert!(rebased.diff.contains_key("layer-b"));
    assert_eq!(rebased.base_revision, store.status().revision);
    assert_eq!(store.take_activities().len(), 1);
    store.record_delivery(&tuple(), 2, true).unwrap();
    assert_eq!(store.status().base_revision, rebased.base_revision);
    store
        .commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 3),
            snapshot(2, 3),
            1,
        )
        .unwrap();
    assert!(store.body().unwrap().proposal.is_none());
}

#[test]
fn config_store_history_bounds_protected_versions_pruned_audit_and_rollback() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), 1, true).unwrap();
    commit(&mut store, CommitKind::Approved);
    store.record_delivery(&tuple(), 2, true).unwrap();
    for _ in 0..30 {
        commit(&mut store, CommitKind::Approved);
    }
    let body = store.body().unwrap();
    assert_eq!(body.generations.len(), RECENT_HISTORY_LIMIT + 3);
    assert!(body.generations.contains_key(&1)); // recovery
    assert!(body.generations.contains_key(&2)); // active
    assert!(body.generations.contains_key(&32)); // pending
    assert_eq!(body.pruned[&3].generation, 3);
    assert_eq!(body.pruned[&3].pruned_at_ms, 12);
    error_code(
        store.rollback_configuration(3),
        StoreErrorCode::GenerationPruned,
    );
    assert_eq!(store.rollback_configuration(1).unwrap(), snapshot(1, 1));
    let mut restarted = rig.open(false);
    assert!(restarted.body().unwrap().pruned.contains_key(&3));
    assert_eq!(commit(&mut restarted, CommitKind::Rollback), 33);
    assert_eq!(commit(&mut restarted, CommitKind::Development), 34);
    assert_eq!(restarted.body().unwrap().versions[0].1.active, Some(2));
}

#[test]
fn config_store_atomic_owner_only_and_failed_staging_keeps_previous() {
    let rig = Rig::new();
    let path = rig.path.join(STORE_FILE);
    disk::publish(&path, b"previous", |_| Ok(())).unwrap();
    assert!(
        disk::publish(&path, b"next", |p| if p == disk::PausePoint::BeforeRename {
            Err(io::Error::other("killed before rename"))
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), b"previous");
    disk::publish(&path, b"next", |_| Ok(())).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"next");
    assert_eq!(fs::read_dir(&rig.path).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&rig.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

fn byte_limit_entry(kind: CommitKind) -> HistoryEntry {
    let mut entry = entry(kind);
    entry.configuration.insert(
        "history-padding".into(),
        RecordedValue::new(serde_json::json!("p".repeat(4096))),
    );
    entry
}

fn daemon_byte_limit_entry(kind: CommitKind, version: &str, padding: usize) -> HistoryEntry {
    use subc_protocol::config::{ConfigLayer, ConfigLayerSnapshot};
    let mut entry = entry(kind);
    entry.tuples.clear();
    entry.generation.development = kind == CommitKind::Development;
    entry.generation.input_vector = InputVector::new(BTreeMap::from([(
        "subc.jsonc".into(),
        subc_config::InputState::Present {
            digest: raw_digest(format!("bound-file-{version}").as_bytes()),
        },
    )]));
    let bytes = format!(
        "// config version {version}\n{{\"version\":1,\"modules\":{{}}}}\n{}",
        " ".repeat(padding)
    )
    .into_bytes();
    entry.delivery = Some(DeliverySnapshot {
        daemon: Some(ConfigLayerSnapshot::file(
            "subc",
            ConfigLayer::User,
            "/approved/subc.jsonc",
            Some(bytes),
        )),
        modules: BTreeMap::new(),
        declarations: BTreeMap::new(),
    });
    entry
}

#[test]
fn config_store_byte_limit_prunes_recent_history_before_publish_and_stays_readable() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    store
        .commit_generation(
            byte_limit_entry(CommitKind::Stamp),
            snapshot(1, 1),
            snapshot(1, 1),
            10,
        )
        .unwrap();
    store.record_delivery(&tuple(), 1, true).unwrap();
    store
        .commit_generation(
            byte_limit_entry(CommitKind::Approved),
            snapshot(1, 1),
            snapshot(1, 1),
            11,
        )
        .unwrap();
    store.record_delivery(&tuple(), 2, true).unwrap();
    store
        .commit_generation(
            byte_limit_entry(CommitKind::Approved),
            snapshot(1, 1),
            snapshot(1, 1),
            12,
        )
        .unwrap();
    let limit = rig.bytes().len() + 512;
    store.byte_limit = limit;
    let id = store
        .commit_generation(
            byte_limit_entry(CommitKind::Approved),
            snapshot(1, 1),
            snapshot(1, 1),
            42,
        )
        .unwrap();
    assert_eq!(id, 4);
    assert!(
        rig.bytes().len() <= limit,
        "publication exceeded its configured limit"
    );
    let body = store.body().unwrap();
    assert_eq!(
        body.generations.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(body.pruned[&3].plan_digest, raw_digest(b"approved-plan"));
    assert_eq!(body.pruned[&3].approval_id, "approval");
    assert_eq!(body.pruned[&3].pruned_at_ms, 42);
    let mut restarted = rig.open(false);
    let reopened = restarted.body().unwrap();
    assert_eq!(reopened.versions[0].1.active, Some(2));
    assert_eq!(reopened.versions[0].1.recovery, Some(1));
    assert_eq!(reopened.versions[0].1.pending, Some(4));
    assert!(reopened.pruned.contains_key(&3));
}

#[test]
fn config_store_byte_limit_refuses_unprunable_candidate_without_changing_bytes_or_anchor() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), 1, true).unwrap();
    let before = rig.bytes();
    let anchor = rig.key.lock().unwrap().anchor;
    let body_before = serde_json::to_value(store.body().unwrap()).unwrap();
    store.byte_limit = before.len() + 512;
    let error = store
        .commit_generation(
            byte_limit_entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            45,
        )
        .unwrap_err();
    assert_eq!(error.code, StoreErrorCode::StoreTooLarge);
    assert!(error.detail.contains("STORE_BYTE_LIMIT"));
    assert!(error.detail.contains(&store.byte_limit.to_string()));
    assert_eq!(
        crate::config_gate::GateError::from(error).code,
        "store_too_large"
    );
    assert_eq!(rig.bytes(), before);
    assert_eq!(rig.key.lock().unwrap().anchor, anchor);
    assert_eq!(
        serde_json::to_value(store.body().unwrap()).unwrap(),
        body_before
    );
    store.delivery_ready().unwrap();
    assert_eq!(rig.open(true).body().unwrap().last_generation, 1);
}

#[test]
fn config_store_byte_limit_first_daemon_stamp_cannot_prune_its_own_snapshot() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    store.byte_limit = 1024;
    let entry = daemon_byte_limit_entry(CommitKind::Stamp, "A", 4096);
    let raw = entry
        .delivery
        .as_ref()
        .unwrap()
        .daemon
        .as_ref()
        .unwrap()
        .bytes
        .clone();
    error_code(
        store.commit_generation(entry.clone(), snapshot(1, 1), snapshot(1, 1), 1),
        StoreErrorCode::StoreTooLarge,
    );
    assert!(!store.path().exists());
    assert!(store.body().unwrap().generations.is_empty());
    store.byte_limit = STORE_BYTE_LIMIT;
    store
        .commit_generation(entry, snapshot(1, 1), snapshot(1, 1), 2)
        .unwrap();
    let mut restarted = rig.open(false);
    let body = restarted.body().unwrap();
    assert_eq!(
        body.generations[&1]
            .delivery
            .as_ref()
            .unwrap()
            .daemon
            .as_ref()
            .unwrap()
            .bytes,
        raw
    );
    assert!(body.pruned.is_empty());
}

#[test]
fn config_store_byte_limit_second_daemon_change_keeps_active_and_recovery() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    store
        .commit_generation(
            daemon_byte_limit_entry(CommitKind::Stamp, "A", 4096),
            snapshot(1, 1),
            snapshot(1, 1),
            1,
        )
        .unwrap();
    store
        .commit_generation(
            byte_limit_entry(CommitKind::Development),
            snapshot(1, 1),
            snapshot(1, 1),
            2,
        )
        .unwrap();
    let limit = rig.bytes().len() + 4096;
    store.byte_limit = limit;
    store
        .commit_generation(
            daemon_byte_limit_entry(CommitKind::Approved, "B", 4096),
            snapshot(2, 1),
            snapshot(2, 1),
            3,
        )
        .unwrap();
    assert!(rig.bytes().len() <= limit);
    let body = store.body().unwrap();
    assert_eq!(
        body.generations.keys().copied().collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert!(body.pruned.contains_key(&2));
    assert_eq!(body.protected_generations(), BTreeSet::from([1, 3]));
    assert_eq!(
        rig.open(false).body().unwrap().protected_generations(),
        BTreeSet::from([1, 3])
    );
}

#[test]
fn config_store_byte_limit_groups_daemon_versions_across_rollbacks_and_module_only_carriers() {
    let mut body = StoreBody::default();
    for (id, version, module_only) in [
        (1, "A", false),
        (2, "B", false),
        (3, "B", true),
        (4, "A", false),
        (5, "A", true),
    ] {
        let mut entry = daemon_byte_limit_entry(
            if id == 1 {
                CommitKind::Stamp
            } else {
                CommitKind::Approved
            },
            version,
            0,
        );
        entry.generation.id = id;
        if module_only {
            entry.tuples.insert(tuple());
        }
        body.generations.insert(id, entry);
    }
    for id in 6..36 {
        let mut entry = daemon_byte_limit_entry(CommitKind::Development, "C", 0);
        entry.generation.id = id;
        body.generations.insert(id, entry);
    }
    assert_eq!(body.protected_generations(), BTreeSet::from([3, 5]));
    body.prune(50);
    assert!(body.generations.contains_key(&3));
    assert!(body.generations.contains_key(&5));
    assert!(!body.generations.contains_key(&4));
    while body.prune_oldest(60) {}
    assert_eq!(
        body.generations.keys().copied().collect::<Vec<_>>(),
        vec![3, 5]
    );
    assert_eq!(body.pruned[&1].generation, 1);
    assert_eq!(body.pruned[&2].generation, 2);
    assert_eq!(body.pruned[&4].generation, 4);
}

#[test]
fn config_store_sealed_failure_replay_and_start_anchor_retry() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    let pending = rig.bytes();
    store.record_delivery(&tuple(), 1, false).unwrap();
    let failed = rig.bytes();
    rig.replace(&pending);
    assert_eq!(rig.open(true).status().state, StoreState::StoreUnverified);
    rig.replace(&failed);
    let mut store = rig.open(true);
    assert!(!store.resume_delivery(&tuple(), 1).unwrap());
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            1,
        ),
        StoreErrorCode::AnchorPending,
    );
    let mut restarted = rig.open(true);
    assert_eq!(restarted.status().state, StoreState::Stamped);
    assert!(restarted.status().anchor_pending);
    error_code(restarted.delivery_ready(), StoreErrorCode::AnchorPending);
    rig.key.lock().unwrap().fail = false;
    restarted.retry_anchor().unwrap();
    let before = restarted.status();
    assert!(restarted.resume_delivery(&tuple(), 2).unwrap());
    assert_eq!(restarted.status().revision, before.revision + 1);
    assert_eq!(restarted.status().base_revision, before.base_revision);
}

#[test]
fn config_store_fresh_digest_stamp_resets_exhausted_revision() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    commit(&mut store, CommitKind::Stamp);
    commit(&mut store, CommitKind::Unstamp);
    let mut envelope: Envelope = serde_json::from_slice(&rig.bytes()).unwrap();
    envelope.revision = u64::MAX;
    rig.replace(&serde_json::to_vec(&envelope).unwrap());
    let mut restarted = rig.open(false);
    assert_eq!(commit(&mut restarted, CommitKind::Stamp), 3);
    assert_eq!(restarted.status().revision, 1);
}

#[test]
fn config_store_unstamped_retained_edits_deletion_and_stamped_bit_are_not_authority() {
    for sealed in [false, true] {
        let rig = Rig::new();
        let mut store = rig.open(sealed);
        commit(&mut store, CommitKind::Stamp);
        commit(&mut store, CommitKind::Unstamp);
        let mut retained: Envelope = serde_json::from_slice(&rig.bytes()).unwrap();
        retained.body.stamped = true;
        retained.body.approved = snapshot(666, 666);
        rig.replace(&serde_json::to_vec(&retained).unwrap());
        assert_eq!(store.status().state, StoreState::Unstamped);
        assert_eq!(store.body().unwrap().approved, snapshot(1, 1));
        rig.replace(b"arbitrary edit");
        assert_eq!(store.status().state, StoreState::Unstamped);
        fs::remove_file(rig.path.join(STORE_FILE)).unwrap();
        assert_eq!(store.status().state, StoreState::Unstamped);
        assert!(store
            .take_activities()
            .iter()
            .all(|a| a.event != "config.tamper"));
        store
            .commit_generation(entry(CommitKind::Stamp), snapshot(8, 9), snapshot(8, 9), 1)
            .unwrap();
        assert_eq!(store.body().unwrap().approved, snapshot(8, 9));
        assert_eq!(store.status().revision, if sealed { 3 } else { 1 });
        rig.replace(b"same edit while stamped");
        error_code(store.check(), StoreErrorCode::StoreTampered);
    }
}

#[test]
fn config_store_sealed_rechecks_external_anchor_on_every_use() {
    for unstamp in [false, true] {
        let rig = Rig::new();
        let mut store = rig.open(true);
        commit(&mut store, CommitKind::Stamp);
        if unstamp {
            commit(&mut store, CommitKind::Unstamp);
        }
        rig.key.lock().unwrap().denied = true;
        error_code(store.body(), StoreErrorCode::StoreUnverified);
        assert_eq!(store.status().state, StoreState::StoreUnverified);
    }
}

#[test]
fn config_store_unstamp_anchor_pending_keeps_gate_until_clear_commits() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Unstamp),
            snapshot(1, 1),
            snapshot(1, 1),
            1,
        ),
        StoreErrorCode::AnchorPending,
    );
    assert_eq!(store.status().state, StoreState::Stamped);
    let mut restarted = rig.open(true);
    assert_eq!(restarted.status().state, StoreState::Stamped);
    rig.key.lock().unwrap().fail = false;
    restarted.retry_anchor().unwrap();
    assert_eq!(restarted.status().state, StoreState::Unstamped);
}

#[test]
fn config_store_unanchored_mac_covers_unknown_fields_and_exact_body_bytes() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    rig.key.lock().unwrap().fail = true;
    error_code(
        store.commit_generation(
            entry(CommitKind::Approved),
            snapshot(2, 1),
            snapshot(2, 1),
            1,
        ),
        StoreErrorCode::AnchorPending,
    );
    let original = String::from_utf8(rig.bytes()).unwrap();
    rig.key.lock().unwrap().fail = false;
    for replacement in ["\"body\":{\"unknown\":true,", "\"body\":{ "] {
        let edited = original.replacen("\"body\":{", replacement, 1);
        assert_ne!(edited, original);
        rig.replace(edited.as_bytes());
        assert_eq!(rig.open(true).status().state, StoreState::StoreUnverified);
    }
    rig.replace(original.as_bytes());
    assert_eq!(rig.open(true).status().state, StoreState::Stamped);
}
