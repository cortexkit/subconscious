//! Daemon boot generation selection and the durable restart intent.

use super::*;
use subc_config::{ApplyKind, FieldChange};

/// An approved generation carrying daemon bytes. `restart` adds a field that
/// only takes effect when the daemon restarts.
fn daemon_entry(kind: CommitKind, version: &str, restart: bool) -> HistoryEntry {
    let mut entry = daemon_byte_limit_entry(kind, version, 0);
    entry.tuples = BTreeSet::from([tuple()]);
    if restart {
        entry.generation.changeset.fields.push(FieldChange::new(
            DAEMON_MODULE,
            "port",
            RecordedValue::new(serde_json::json!(1)),
            RecordedValue::new(serde_json::json!(2)),
            ApplyKind::DaemonRestart,
        ));
    }
    entry
}

fn approve(store: &mut ConfigStore, kind: CommitKind, version: &str, restart: bool) -> u64 {
    store
        .commit_generation(
            daemon_entry(kind, version, restart),
            snapshot(1, 1),
            snapshot(1, 1),
            12,
        )
        .unwrap()
}

#[test]
fn config_daemon_boot_skips_newest_generation_awaiting_restart() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    assert_eq!(approve(&mut store, CommitKind::Stamp, "1", false), 1);
    assert_eq!(approve(&mut store, CommitKind::Approved, "2", true), 2);
    // Approved bytes are cumulative: a later module-only approval still
    // carries the port change that waits for a daemon restart.
    assert_eq!(approve(&mut store, CommitKind::Approved, "3", false), 3);
    let mut restarted = rig.open(false);
    assert_eq!(restarted.select_boot_generation().unwrap(), 1);
    assert_eq!(restarted.booted_generation(), Some(1));
    assert_eq!(restarted.body().unwrap().restart_candidate(), Some(3));
}

#[test]
fn config_daemon_boot_takes_a_committed_generation_without_restart_fields() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    approve(&mut store, CommitKind::Stamp, "1", false);
    approve(&mut store, CommitKind::Approved, "2", false);
    assert_eq!(rig.open(false).select_boot_generation().unwrap(), 2);
    assert_eq!(store.body().unwrap().restart_candidate(), None);
}

#[test]
fn config_daemon_boot_never_selects_a_failed_newest_generation() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    approve(&mut store, CommitKind::Stamp, "1", false);
    store.record_delivery(&tuple(), 1, true).unwrap();
    approve(&mut store, CommitKind::Approved, "2", false);
    store.record_delivery(&tuple(), 2, false).unwrap();
    let mut restarted = rig.open(false);
    assert_eq!(restarted.select_boot_generation().unwrap(), 1);
    assert_eq!(restarted.body().unwrap().restart_candidate(), None);
}

#[test]
fn config_daemon_restart_intent_warms_up_once_and_readiness_activates_it() {
    for sealed in [false, true] {
        let rig = Rig::new();
        let mut store = rig.open(sealed);
        approve(&mut store, CommitKind::Stamp, "1", false);
        approve(&mut store, CommitKind::Approved, "2", true);
        assert_eq!(store.assign_daemon_restart().unwrap(), Some(2));
        drop(store);
        let mut warming = rig.open(sealed);
        assert_eq!(warming.select_boot_generation().unwrap(), 2);
        assert_eq!(
            warming.body().unwrap().daemon.restart,
            Some(RestartIntent {
                generation: 2,
                attempted: true
            }),
            "the attempt is durable before the generation boots"
        );
        warming.record_daemon_ready().unwrap();
        assert_eq!(
            warming.body().unwrap().daemon,
            DaemonPointers {
                active: 2,
                awaiting_restart: None,
                restart: None
            }
        );
        drop(warming);
        let mut ordinary = rig.open(sealed);
        assert_eq!(ordinary.select_boot_generation().unwrap(), 2);
        assert_eq!(ordinary.assign_daemon_restart().unwrap(), None);
    }
}

#[test]
fn config_daemon_restart_boot_without_readiness_fails_and_falls_back() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    approve(&mut store, CommitKind::Stamp, "1", false);
    approve(&mut store, CommitKind::Approved, "2", true);
    store.assign_daemon_restart().unwrap();
    drop(store);
    // The warm-up boots and then dies before it records readiness.
    assert_eq!(rig.open(false).select_boot_generation().unwrap(), 2);
    let mut next = rig.open(false);
    assert_eq!(next.select_boot_generation().unwrap(), 1);
    let body = next.body().unwrap();
    assert_eq!(body.daemon.restart, None);
    assert!(body.generations[&2]
        .generation
        .never_redeliver
        .contains(&DAEMON_MODULE.into()));
    assert_eq!(next.assign_daemon_restart().unwrap(), None);
    drop(next);
    assert_eq!(rig.open(false).select_boot_generation().unwrap(), 1);
}

#[test]
fn config_store_present_but_malformed_opens_in_recovery_without_a_sealed_anchor() {
    let rig = Rig::new();
    rig.replace(b"never a store");
    assert_eq!(rig.open(false).status().state, StoreState::StoreUnverified);
    let store = Rig::new();
    let mut stamped = store.open(false);
    approve(&mut stamped, CommitKind::Stamp, "1", false);
    let valid: serde_json::Value = serde_json::from_slice(&store.bytes()).unwrap();
    let mut without_pointers = valid.clone();
    without_pointers["body"]
        .as_object_mut()
        .unwrap()
        .remove("daemon")
        .unwrap();
    let mut revision_zero = valid;
    revision_zero["revision"] = serde_json::json!(0);
    for malformed in [
        serde_json::to_vec(&without_pointers).unwrap(),
        serde_json::to_vec(&revision_zero).unwrap(),
        b"{\"format_version\":1".to_vec(),
    ] {
        store.replace(&malformed);
        let mut reopened = store.open(false);
        assert_eq!(reopened.status().state, StoreState::StoreUnverified);
        error_code(reopened.body(), StoreErrorCode::StoreUnverified);
    }
}
