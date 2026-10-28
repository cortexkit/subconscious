#![cfg(target_os = "linux")]

use std::{fs, io, os::unix::fs::symlink, path::PathBuf, time::Duration};

use cortexkit_test_support::ScratchDir;
use subc_cgroup::{freeze_module, prepare_at, FreezeOutcome, Placement, ProcsOutcome, ThawOutcome};

fn fixture(name: &str) -> (ScratchDir, Placement, PathBuf) {
    let root = ScratchDir::new(name);
    fs::write(root.join("cgroup.procs"), "").unwrap();
    let placement = prepare_at(&root).unwrap().unwrap();
    let module = placement.module_path("module").unwrap();
    (root, placement, module)
}

#[tokio::test]
async fn freeze_distinguishes_absent_placement_support_and_io_errors() {
    let (_root, placement, module) = fixture("cgroup-freeze-outcomes");
    assert!(matches!(
        freeze_module(None, "module", Duration::ZERO).await,
        FreezeOutcome::NotPlaced
    ));
    assert!(matches!(
        freeze_module(Some(&placement), "absent", Duration::ZERO).await,
        FreezeOutcome::NotPlaced
    ));
    assert!(matches!(
        freeze_module(Some(&placement), "module", Duration::ZERO).await,
        FreezeOutcome::Unsupported
    ));
    assert!(!module.join("cgroup.freeze").exists());
    fs::create_dir(module.join("cgroup.freeze")).unwrap();
    assert!(matches!(
        freeze_module(Some(&placement), "module", Duration::ZERO).await,
        FreezeOutcome::IoError { path, .. } if path == module.join("cgroup.freeze")
    ));
    for id in ["", ".", ".."] {
        assert!(matches!(
            freeze_module(Some(&placement), id, Duration::ZERO).await,
            FreezeOutcome::IoError { error, .. } if error.kind() == io::ErrorKind::InvalidInput
        ));
    }
}

#[tokio::test]
async fn freeze_waits_for_confirmation_and_thaws_explicitly_or_on_drop() {
    let (_root, placement, module) = fixture("cgroup-freeze-confirmation");
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    let confirmation = async {
        while fs::read(module.join("cgroup.freeze")).unwrap() != b"1" {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        fs::write(module.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
    };
    let (outcome, ()) = tokio::join!(
        freeze_module(Some(&placement), "module", Duration::from_secs(1)),
        confirmation
    );
    let FreezeOutcome::Frozen(frozen) = outcome else {
        panic!("expected confirmed freeze, got {outcome:?}");
    };
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"1");
    assert!(matches!(frozen.thaw(), ThawOutcome::Thawed));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    let outcome = freeze_module(Some(&placement), "module", Duration::ZERO).await;
    assert!(matches!(outcome, FreezeOutcome::Frozen(_)));
    drop(outcome);
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
}

#[tokio::test]
async fn failed_or_cancelled_freeze_wait_releases_the_request() {
    let (_root, placement, module) = fixture("cgroup-freeze-failure");
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    assert!(matches!(
        freeze_module(Some(&placement), "module", Duration::ZERO).await,
        FreezeOutcome::TimedOut { .. }
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    assert!(tokio::time::timeout(
        Duration::from_millis(5),
        freeze_module(Some(&placement), "module", Duration::from_secs(60))
    )
    .await
    .is_err());
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    fs::remove_file(module.join("cgroup.events")).unwrap();
    assert!(matches!(
        freeze_module(Some(&placement), "module", Duration::ZERO).await,
        FreezeOutcome::IoError { path, .. } if path == module.join("cgroup.events")
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
}

#[tokio::test]
async fn frozen_process_listing_includes_descendants_but_not_siblings_or_symlinks() {
    let (root, placement, module) = fixture("cgroup-frozen-procs");
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
    fs::write(module.join("cgroup.procs"), "0\n1\n-1\n42\n").unwrap();
    let nested = module.join("child/grandchild");
    fs::create_dir_all(&nested).unwrap();
    fs::write(module.join("child/cgroup.procs"), "43\n").unwrap();
    fs::write(nested.join("cgroup.procs"), "44\n42\n").unwrap();
    let sibling = placement.module_path("sibling").unwrap();
    fs::write(sibling.join("cgroup.procs"), "999\n").unwrap();
    symlink(&sibling, module.join("outside")).unwrap();
    fs::write(root.join("cgroup.procs"), "888\n").unwrap();
    let FreezeOutcome::Frozen(frozen) =
        freeze_module(Some(&placement), "module", Duration::ZERO).await
    else {
        panic!("freeze should be confirmed");
    };
    assert!(matches!(frozen.processes(), ProcsOutcome::Listed(pids) if pids == [42, 43, 44]));
    fs::write(nested.join("cgroup.procs"), "invalid\n").unwrap();
    assert!(matches!(
        frozen.processes(),
        ProcsOutcome::IoError { path, error }
            if path == nested.join("cgroup.procs") && error.kind() == io::ErrorKind::InvalidData
    ));
    fs::remove_file(nested.join("cgroup.procs")).unwrap();
    assert!(matches!(
        frozen.processes(),
        ProcsOutcome::IoError { path, .. } if path == nested.join("cgroup.procs")
    ));
}

#[tokio::test]
async fn thaw_reports_the_failed_interface_path() {
    let (_root, placement, module) = fixture("cgroup-thaw-error");
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "frozen 1\n").unwrap();
    let FreezeOutcome::Frozen(frozen) =
        freeze_module(Some(&placement), "module", Duration::ZERO).await
    else {
        panic!("freeze should be confirmed");
    };
    fs::remove_file(module.join("cgroup.freeze")).unwrap();
    assert!(matches!(
        frozen.thaw(), ThawOutcome::IoError { path, .. } if path == module.join("cgroup.freeze")
    ));
}
