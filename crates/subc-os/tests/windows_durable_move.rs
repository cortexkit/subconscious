#![cfg(windows)]

use cortexkit_test_support::ScratchDir;
use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore = "scratch executable mapped by the parent test"]
fn mapped_image_child() {
    let marker = std::env::var_os("CK_TEST_DURABLE_MOVE_READY").unwrap();
    fs::write(marker, "running").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn durable_move_never_replaces_a_destination_and_moves_a_running_source() {
    let fixture = ScratchDir::new("durable-mapped-move");
    let source = fixture.join("mapped.exe");
    let target = fixture.join("retired.exe");
    let marker = fixture.join("ready");
    fs::copy(std::env::current_exe().unwrap(), &source).unwrap();
    fs::write(&target, "unrelated file").unwrap();
    let mut child = Command::new(&source)
        .args(["--exact", "mapped_image_child", "--ignored"])
        .env("CK_TEST_DURABLE_MOVE_READY", &marker)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "scratch executable failed to start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let collision = subc_os::durable_move_to_new_name(&source, &target);
    let original_target = fs::read_to_string(&target);
    fs::remove_file(&target).unwrap();
    let moved = subc_os::durable_move_to_new_name(&source, &target);
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(marker.is_file(), "source did not start");
    assert!(collision.is_err());
    assert_eq!(original_target.unwrap(), "unrelated file");
    moved.unwrap();
    assert!(still_running);
    assert!(!source.exists());
    assert!(target.is_file());
}
