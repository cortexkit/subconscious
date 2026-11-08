use super::*;
use crate::config_store::{ConfigStore, StoreRoot, StoreState};
#[cfg(windows)]
use std::fs::File;
use std::path::PathBuf;
#[cfg(unix)]
use std::{
    process::Command,
    time::{Duration, Instant},
};

struct Rig(PathBuf);
impl Rig {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "subc-store-read-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn path(&self) -> PathBuf {
        self.0.join(super::super::STORE_FILE)
    }
    fn file(&self, name: &str) -> File {
        let path = self.0.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap()
        }
        #[cfg(windows)]
        {
            subc_os::windows_acl::create_private_file(&path).unwrap()
        }
    }
    fn assert_unverified(&self) {
        let mut store = ConfigStore::open_digest_only_for_tests(
            StoreRoot::from_service_definition(self.0.clone()).unwrap(),
        );
        assert_eq!(store.status().state, StoreState::StoreUnverified);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn config_store_disk_reads_normal_file_and_preserves_absence() {
    let rig = Rig::new();
    assert_eq!(read(&rig.path()).unwrap(), None);
    rig.file(super::super::STORE_FILE)
        .write_all(b"ordinary store bytes")
        .unwrap();
    assert_eq!(
        read(&rig.path()).unwrap(),
        Some(b"ordinary store bytes".to_vec())
    );
}

#[test]
fn config_store_disk_refuses_symlink_as_unverified() {
    let rig = Rig::new();
    rig.file("target").write_all(b"{}").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(rig.0.join("target"), rig.path()).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(rig.0.join("target"), rig.path()).unwrap();
    assert!(read(&rig.path()).is_err());
    rig.assert_unverified();
}

#[test]
fn config_store_disk_refuses_one_byte_over_limit_as_unverified() {
    let rig = Rig::new();
    rig.file(super::super::STORE_FILE)
        .set_len(STORE_READ_LIMIT + 1)
        .unwrap();
    assert!(read(&rig.path())
        .unwrap_err()
        .to_string()
        .contains("64 MiB"));
    rig.assert_unverified();
}

#[cfg(unix)]
#[test]
fn config_store_disk_fifo_probe() {
    let Some(root) = std::env::var_os("SUBC_STORE_READ_TEST_ROOT") else {
        return;
    };
    let rig = Rig(PathBuf::from(root));
    assert!(read(&rig.path()).is_err());
    rig.assert_unverified();
}

#[cfg(unix)]
#[test]
fn config_store_disk_refuses_fifo_without_blocking_as_unverified() {
    let rig = Rig::new();
    let status = Command::new("mkfifo").arg(rig.path()).status().unwrap();
    assert!(status.success());
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(rig.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config_store::disk::tests::config_store_disk_fifo_probe",
        ])
        .env("SUBC_STORE_READ_TEST_ROOT", &rig.0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "store FIFO probe failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("authority-store read blocked on a substituted FIFO");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
