#![cfg(target_os = "linux")]

use cortexkit_test_support::ScratchDir;
use std::{fs, io, os::unix::fs::symlink, path::PathBuf, time::Duration};
use subc_cgroup::{
    freeze_path, freeze_path_blocking, kill_path, prepare_at, FreezeOutcome, KillOutcome,
    Placement, ProcsOutcome, ThawOutcome,
};

fn fixture(label: &str) -> (ScratchDir, Placement, PathBuf) {
    let root = ScratchDir::new(label);
    fs::write(root.join("cgroup.procs"), "").unwrap();
    let placement = prepare_at(&root).unwrap().unwrap();
    let module = placement.module_path("owned").unwrap();
    (root, placement, module)
}

#[tokio::test]
async fn recorded_path_operations_never_create_missing_cgroups_or_interfaces() {
    let (_root, placement, module) = fixture("recorded-cgroup-missing");
    let absent = module.parent().unwrap().join("m-missing");
    for path in [&absent, &module] {
        let missing = path == &absent;
        assert!(matches!(kill_path(&placement, path), KillOutcome::NotPlaced) == missing);
        assert!(
            matches!(
                freeze_path(&placement, path, Duration::ZERO).await,
                FreezeOutcome::NotPlaced
            ) == missing
        );
        assert!(
            matches!(
                freeze_path_blocking(&placement, path, Duration::ZERO),
                FreezeOutcome::NotPlaced
            ) == missing
        );
    }
    assert!(
        !absent.exists(),
        "a recorded missing container must not be recreated"
    );
    for interface in ["cgroup.kill", "cgroup.freeze", "cgroup.events"] {
        assert!(
            !module.join(interface).exists(),
            "lookup must never manufacture a kernel interface"
        );
    }
    assert!(matches!(
        kill_path(&placement, &module),
        KillOutcome::Unsupported
    ));
    assert!(matches!(
        freeze_path_blocking(&placement, &module, Duration::ZERO),
        FreezeOutcome::Unsupported
    ));
    placement.remove_path(&absent).unwrap();
    placement.remove_path(&module).unwrap();
    placement.remove_path(&module).unwrap();
    assert!(!module.exists());
}

#[tokio::test]
async fn recorded_path_refuses_other_roots_dot_components_and_symlinks() {
    let (root, placement, module) = fixture("recorded-cgroup-root-guard");
    let outside = root.join("unrelated-container");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("cgroup.kill"), "untouched").unwrap();
    fs::write(outside.join("cgroup.freeze"), "untouched").unwrap();
    fs::write(outside.join("cgroup.events"), "frozen 1\n").unwrap();
    let modules = module.parent().unwrap();
    let escaped = modules.join("../unrelated-container");
    let link = modules.join("m-link");
    symlink(&outside, &link).unwrap();
    let inside_link = modules.join("m-inside-link");
    symlink(&module, &inside_link).unwrap();
    let noncanonical = PathBuf::from(format!("{}/./m-owned", modules.display()));
    for path in [
        &outside,
        &escaped,
        &link,
        &inside_link,
        &noncanonical,
        modules,
    ] {
        assert!(
            matches!(kill_path(&placement, path), KillOutcome::IoError { error, .. } if error.kind() == io::ErrorKind::InvalidInput),
            "must refuse {}",
            path.display()
        );
        assert!(
            matches!(freeze_path(&placement, path, Duration::ZERO).await, FreezeOutcome::IoError { error, .. } if error.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(freeze_path_blocking(&placement, path, Duration::ZERO), FreezeOutcome::IoError { error, .. } if error.kind() == io::ErrorKind::InvalidInput)
        );
        assert_eq!(
            placement.remove_path(path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        fs::read_to_string(outside.join("cgroup.kill")).unwrap(),
        "untouched"
    );
    assert_eq!(
        fs::read_to_string(outside.join("cgroup.freeze")).unwrap(),
        "untouched"
    );
    assert!(outside.is_dir());
}

#[tokio::test]
async fn recorded_path_freeze_confirms_lists_and_thaws_in_async_and_blocking_cleanup() {
    let (_root, placement, module) = fixture("recorded-cgroup-freeze");
    fs::write(module.join("cgroup.kill"), "0").unwrap();
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
    fs::write(module.join("cgroup.procs"), "42\n").unwrap();
    let child = module.join("child");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("cgroup.procs"), "43\n").unwrap();
    assert_eq!(
        placement.existing_module_path(&module).unwrap(),
        Some(module.clone())
    );
    let FreezeOutcome::Frozen(frozen) = freeze_path(&placement, &module, Duration::ZERO).await
    else {
        panic!("must confirm the asynchronous freeze")
    };
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"1");
    assert!(matches!(frozen.processes(), ProcsOutcome::Listed(pids) if pids == [42, 43]));
    assert!(matches!(frozen.thaw(), ThawOutcome::Thawed));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    let blocking_placement = placement.clone();
    let blocking_module = module.clone();
    let outcome = std::thread::spawn(move || {
        freeze_path_blocking(&blocking_placement, &blocking_module, Duration::ZERO)
    })
    .join()
    .unwrap();
    assert!(matches!(outcome, FreezeOutcome::Frozen(_)));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"1");
    drop(outcome);
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    assert!(matches!(
        kill_path(&placement, &module),
        KillOutcome::Killed
    ));
    assert_eq!(fs::read(module.join("cgroup.kill")).unwrap(), b"1");
}

#[tokio::test]
async fn recorded_path_failed_freeze_always_releases_its_request() {
    let (_root, placement, module) = fixture("recorded-cgroup-failed-freeze");
    fs::write(module.join("cgroup.freeze"), "0").unwrap();
    fs::write(module.join("cgroup.events"), "frozen 0\n").unwrap();
    assert!(matches!(
        freeze_path(&placement, &module, Duration::ZERO).await,
        FreezeOutcome::TimedOut { .. }
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    assert!(tokio::time::timeout(
        Duration::from_millis(5),
        freeze_path(&placement, &module, Duration::from_secs(60))
    )
    .await
    .is_err());
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    assert!(matches!(
        freeze_path_blocking(&placement, &module, Duration::ZERO),
        FreezeOutcome::TimedOut { .. }
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    fs::remove_file(module.join("cgroup.events")).unwrap();
    assert!(matches!(
        freeze_path(&placement, &module, Duration::ZERO).await,
        FreezeOutcome::IoError { .. }
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
    assert!(matches!(
        freeze_path_blocking(&placement, &module, Duration::ZERO),
        FreezeOutcome::IoError { .. }
    ));
    assert_eq!(fs::read(module.join("cgroup.freeze")).unwrap(), b"0");
}
