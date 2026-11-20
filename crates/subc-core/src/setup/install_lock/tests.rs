use std::{
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use cortexkit_test_support::ScratchDir;
use serde_json::Map;

use super::*;
use crate::setup::inventory::Inventory;

const PLATFORM: &str = "linux-x64";

fn load_for(manifest: &Path, command: &str) -> Inventory {
    Inventory::load_locked(manifest, PLATFORM, command, WAIT).unwrap()
}

/// Two setup runs for different components of one install: each loads the
/// manifest, places its binary and saves. Without the lock the second save
/// wrote back the manifest it loaded before the first save and dropped the
/// first run's ownership row.
#[test]
fn two_installers_keep_both_ownership_rows() {
    let root = ScratchDir::new("install-lock-two-writers");
    let manifest = root.join("installer-manifest.json");
    let first_binary = root.join("bin").join("ck-subc");
    let second_binary = root.join("bin").join("ck-subc-mcp");

    let mut first = load_for(&manifest, "ck setup (first)");
    let (loaded, second_loaded) = mpsc::channel();
    let second = {
        let manifest = manifest.clone();
        let second_binary = second_binary.clone();
        thread::spawn(move || {
            let mut second = load_for(&manifest, "ck setup (second)");
            loaded.send(()).unwrap();
            second.record("managed-binary", &second_binary, Map::new());
            second.save().unwrap();
        })
    };
    // The second run has started; give it time to load if nothing stops it.
    let second_loaded_early = second_loaded
        .recv_timeout(Duration::from_millis(500))
        .is_ok();
    first.record("managed-binary", &first_binary, Map::new());
    first.save().unwrap();
    drop(first);
    second.join().unwrap();

    let reloaded = Inventory::load(&manifest, PLATFORM).unwrap();
    for binary in [&first_binary, &second_binary] {
        assert!(
            reloaded.owns_path("managed-binary", binary),
            "a later save discarded the ownership row for {}",
            binary.display()
        );
    }
    assert!(
        !second_loaded_early,
        "the second installer read the manifest while the first held it"
    );
}

#[test]
fn a_second_command_gives_up_naming_the_one_holding_the_lock() {
    let root = ScratchDir::new("install-lock-refusal");
    let manifest = root.join("installer-manifest.json");
    let held = load_for(&manifest, "ck upgrade");

    let started = Instant::now();
    let error = Inventory::load_locked(&manifest, PLATFORM, "ck setup", Duration::from_millis(300))
        .expect_err("the root is locked");

    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "it did not wait"
    );
    assert!(
        error.contains(&format!("pid {}, ck upgrade", std::process::id())),
        "{error}"
    );
    assert!(
        error.contains(&root.path().display().to_string()),
        "{error}"
    );

    // Dropping the inventory releases the lock and forgets the holder.
    drop(held);
    assert!(!root.join(HOLDER_NAME).exists());
    load_for(&manifest, "ck setup");
}

#[cfg(unix)]
#[test]
fn releasing_the_install_lock_does_not_wait_for_an_inherited_descriptor() {
    let root = ScratchDir::new("install-lock-inherited");
    let manifest = root.join("installer-manifest.json");
    let hold = InstallHold::take(&manifest, "first installer", Duration::ZERO).unwrap();
    let inherited = hold._lock._held._file.try_clone().unwrap();
    drop(hold);
    let second = InstallHold::take(&manifest, "second installer", Duration::ZERO).unwrap();
    drop(inherited);
    drop(second);
}

/// A link left at the holder path must not let the installer write through
/// it into another file, nor delete what it points at.
#[cfg(unix)]
#[test]
fn a_link_at_the_holder_path_never_writes_through() {
    let root = ScratchDir::new("install-lock-holder-link");
    let manifest = root.join("installer-manifest.json");
    let precious = root.join("precious.txt");
    std::fs::write(&precious, "the user's notes").unwrap();
    std::os::unix::fs::symlink(&precious, root.join(HOLDER_NAME)).unwrap();

    drop(load_for(&manifest, "ck setup"));

    assert_eq!(
        std::fs::read_to_string(&precious).unwrap(),
        "the user's notes"
    );
}

/// A link at the lock path is refused rather than opened and locked.
#[cfg(unix)]
#[test]
fn a_link_at_the_lock_path_is_refused() {
    let root = ScratchDir::new("install-lock-lock-link");
    let manifest = root.join("installer-manifest.json");
    let elsewhere = root.join("elsewhere");
    std::fs::write(&elsewhere, "").unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(LOCK_NAME)).unwrap();

    let error = Inventory::load_locked(&manifest, PLATFORM, "ck setup", WAIT)
        .expect_err("a linked lock file is refused");

    assert!(error.contains("is a link"), "{error}");
}

/// The holder note was once staged at the fixed name
/// `installer.lock.holder.tmp`, written through whatever was there. A link
/// left at that name (on Windows a hard link needs no privilege) must not
/// let taking the lock write into the file it points at.
#[test]
fn a_link_at_the_old_holder_staging_name_is_never_written_through() {
    let root = ScratchDir::new("install-lock-staging-link");
    let manifest = root.join("installer-manifest.json");
    let precious = root.join("precious.txt");
    std::fs::write(&precious, "the user's notes").unwrap();
    std::fs::hard_link(&precious, root.join(format!("{HOLDER_NAME}.tmp"))).unwrap();

    drop(load_for(&manifest, "ck setup"));

    assert_eq!(
        std::fs::read_to_string(&precious).unwrap(),
        "the user's notes"
    );
}
