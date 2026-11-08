use std::{collections::BTreeSet, path::PathBuf};
use subc_config::{raw_digest, Changeset, Generation, InputVector};
use subc_daemon::config_store::{
    testing::{PausePoint, TestSupport},
    *,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode");
    let root = PathBuf::from(args.next().expect("isolated root"));
    let sealed = args.next().as_deref() == Some("sealed");
    if mode == "windows-root" {
        println!(
            "{}",
            StoreRoot::installation_default().unwrap().path().display()
        );
        return;
    }
    let hooks = match TestSupport::new(root.clone(), sealed.then(|| root.join("test-key"))) {
        Ok(hooks) => hooks,
        Err(_) => {
            println!("hooks-denied");
            std::process::exit(42);
        }
    };
    let hooks = match mode.as_str() {
        "before-rename" => hooks.with_pause(PausePoint::BeforeRename, root.join("pause")),
        "after-rename" => hooks.with_pause(PausePoint::AfterRename, root.join("pause")),
        "before-anchor" => hooks.with_pause(PausePoint::BeforeAnchor, root.join("pause")),
        "fail-anchor" => hooks.with_anchor_failure(true),
        "deny-key" => hooks.with_key_denial(true),
        _ => hooks,
    };
    let mut store = hooks.open().unwrap();
    if mode == "status" || mode == "deny-key" {
        println!("{:?}", store.status());
        return;
    }
    if mode == "spawn-pause" {
        store
            .test_support()
            .unwrap()
            .clone()
            .with_pause(PausePoint::SpawnBeforeExec, root.join("pause"))
            .pause_spawn_before_exec()
            .unwrap();
        return;
    }
    if mode == "swap-pause" {
        store
            .test_support()
            .unwrap()
            .clone()
            .with_pause(PausePoint::SwapBeforePromotion, root.join("pause"))
            .pause_swap_before_promotion()
            .unwrap();
        return;
    }
    let kind = if store.status().state == StoreState::Unstamped {
        CommitKind::Stamp
    } else {
        CommitKind::Approved
    };
    let entry = HistoryEntry {
        delivery: None,
        generation: Generation::new(
            0,
            raw_digest(b"fixture-plan"),
            InputVector::default(),
            Changeset::new(),
        ),
        kind,
        approval_id: "fixture-approval".into(),
        outcome: "pending".into(),
        tuples: BTreeSet::new(),
        configuration: Snapshot::default(),
    };
    let result = store.commit_generation(entry, Snapshot::default(), Snapshot::default(), 1);
    if mode == "fail-anchor" {
        assert_eq!(result.unwrap_err().code, StoreErrorCode::AnchorPending);
        assert!(store.status().anchor_pending);
        assert_eq!(
            store.delivery_ready().unwrap_err().code,
            StoreErrorCode::AnchorPending
        );
        store.set_test_anchor_failure(false).unwrap();
        store.retry_anchor().unwrap();
        store.delivery_ready().unwrap();
    } else {
        result.unwrap();
    }
    println!("{:?}", store.status());
}
