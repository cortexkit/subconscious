#![cfg(windows)]

use crate::{ExecutableCapture, ImageAgreement, ImageUnavailable, Process, SpawnedImage};
use std::{
    fs,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Child, Command},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

struct Fixture {
    child: Child,
    image: SpawnedImage,
    directory: PathBuf,
}

impl Fixture {
    fn spawn() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "subc-os-image-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("child.exe");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let capture = ExecutableCapture::open(&executable).unwrap();
        // The test binary stays suspended: no recursive test run or helper child
        // can start. Its mapped image and creation time are already observable.
        let child = Command::new(capture.path())
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .unwrap();
        let image = capture.bind(&child).unwrap();
        Self {
            child,
            image,
            directory,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn windows_open_by_pid_refuses_creation_time_mismatch() {
    let mut fixture = Fixture::spawn();
    let process = Process::open(fixture.child.id()).unwrap().unwrap();
    let actual = process.observe().unwrap().start_time;
    let error = process
        .force_stop(actual + 1, Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        fixture.child.try_wait().unwrap().is_none(),
        "mismatched process was killed"
    );
}

#[test]
fn windows_spawned_image_matches_then_detects_path_replacement() {
    let fixture = Fixture::spawn();
    let expected = fixture.image.start_time();
    assert_eq!(
        fixture.image.agreement(expected),
        ImageAgreement::Match(fixture.image.identity())
    );
    let path = fixture.image.path();
    fs::rename(path, fixture.directory.join("old.exe")).unwrap();
    fs::write(path, b"a different file object at the spawn path").unwrap();
    match fixture.image.agreement(expected) {
        ImageAgreement::Mismatch { running, disk } => {
            assert_eq!(running, fixture.image.identity());
            assert_ne!(running, disk);
        }
        other => panic!("replacement was not a mismatch: {other:?}"),
    }
}

#[test]
fn windows_exited_child_image_is_unavailable() {
    let mut fixture = Fixture::spawn();
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    assert_eq!(
        fixture.image.agreement(fixture.image.start_time()),
        ImageAgreement::Unavailable(ImageUnavailable::ProcessExited)
    );
}

#[test]
fn windows_forced_stop_waits_for_exit() {
    let mut fixture = Fixture::spawn();
    let process = fixture.image.process();
    let waits_before = crate::windows::completed_bounded_waits();
    assert!(process
        .force_stop(fixture.image.start_time(), Duration::from_secs(5))
        .unwrap());
    assert_eq!(
        crate::windows::completed_bounded_waits(),
        waits_before + 1,
        "forced stop skipped the bounded exit wait"
    );
    assert!(
        process.wait_for_exit(Duration::ZERO).unwrap(),
        "forced stop returned before exit"
    );
    assert!(fixture.child.try_wait().unwrap().is_some());
}

#[test]
fn windows_capture_denies_rename_until_suspended_child_is_bound() {
    let directory =
        std::env::temp_dir().join(format!("subc-os-capture-lock-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let executable = directory.join("child.exe");
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let capture = ExecutableCapture::open(&executable).unwrap();
    assert!(
        fs::rename(&executable, directory.join("old.exe")).is_err(),
        "capture did not pin spawn path"
    );
    let child = Command::new(capture.path())
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .unwrap();
    let image = capture.bind(&child).unwrap();
    let fixture = Fixture {
        child,
        image,
        directory,
    };
    fs::rename(&executable, fixture.directory.join("old.exe")).unwrap();
}

#[test]
fn windows_resources_are_working_set_and_cpu_not_unix_rss() {
    let process = Process::open(std::process::id()).unwrap().unwrap();
    let usage = process.resource_usage().unwrap();
    assert_eq!(usage.memory_kind, crate::MemoryKind::WindowsWorkingSet);
    assert!(usage.memory_bytes > 0);
    assert_eq!(usage.swap_bytes, None);
}

#[test]
fn windows_graceful_signal_is_not_a_forced_stop() {
    let mut fixture = Fixture::spawn();
    assert_eq!(
        fixture
            .image
            .process()
            .signal(crate::Signal::Terminate)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(fixture.child.try_wait().unwrap().is_none());
}
