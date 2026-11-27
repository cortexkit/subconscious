use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    process::Command,
    thread,
};

use cortexkit_test_support::ScratchDir;
use subc_os::fork_exec_test::{flags_are_close_on_exec, pause_before_exec};

use super::{create, lock_file, Locked};

#[test]
fn window_locks_release_while_a_concurrent_fork_is_still_before_exec() {
    const ID: &str = "19700101-000000-0001";
    let scratch = ScratchDir::new("window-fork-locks");
    let root = scratch.path().canonicalize().unwrap();
    let windows = root.join("windows");
    let dir = windows.join(ID);
    std::fs::create_dir_all(&dir).unwrap();
    create(&dir, ID, 0, None).unwrap();

    let registry_path = windows.join(".lock");
    let registry = lock_file(&registry_path).unwrap();
    let window = Locked::open(&root, ID).unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exit 0"]);
    let (mut ready, mut release) = pause_before_exec(&mut command, registry.0.as_raw_fd()).unwrap();
    let spawn = thread::spawn(move || command.status());

    let mut observation = [0; 8];
    let observed = ready.read_exact(&mut observation);
    let registry_held = lock_file(&registry_path).err();
    let window_held = Locked::open(&root, ID).err();
    drop(window);
    drop(registry);
    let registry_after_drop = lock_file(&registry_path).err();
    let window_after_drop = Locked::open(&root, ID).err();

    // Release and reap the child before asserting, so a failure doesn't leave it paused.
    let released = release.write_all(&[1]);
    let status = spawn.join().unwrap().unwrap();
    observed.unwrap();
    released.unwrap();
    assert!(status.success());
    let flags = i32::from_ne_bytes(observation[4..].try_into().unwrap());
    assert!(flags_are_close_on_exec(flags));
    for held in [registry_held, window_held] {
        assert!(
            held.as_deref()
                .is_some_and(|error| error.contains("another window command holds the lock")),
            "expected a lock conflict while its guard lived, got {held:?}"
        );
    }
    assert!(
        registry_after_drop.is_none() && window_after_drop.is_none(),
        "the paused child retained a dropped lock: registry={registry_after_drop:?}, window={window_after_drop:?}"
    );
}
