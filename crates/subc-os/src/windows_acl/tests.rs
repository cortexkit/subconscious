use super::{test_support::*, *};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "subc-acl-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn created_file_has_exact_protected_user_dacl_before_first_write() {
    let root = TempDir::new();
    grant_everyone(&root.0);
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    assert_owner_only(&path, false, true);
    verify_owner_only(&file).unwrap();
    assert!(
        create_private_file(&path).is_err(),
        "never open an existing path"
    );
}

#[test]
fn created_directories_have_exact_protected_user_dacls() {
    let root = TempDir::new();
    grant_everyone(&root.0);
    let parent = root.0.join("parent");
    let path = parent.join("run");
    create_private_dir(&path).unwrap();
    assert_owner_only(&parent, true, true);
    assert_owner_only(&path, true, true);
    let child = path.join("inherited.txt");
    std::fs::write(&child, b"private").unwrap();
    assert_owner_only(&child, false, false);
}

#[test]
fn existing_directory_narrows_unprotected_descendants() {
    let root = TempDir::new();
    grant_everyone(&root.0);
    let run = root.0.join("run");
    let nested = run.join("logs");
    std::fs::create_dir_all(&nested).unwrap();
    let child = nested.join("old.log");
    std::fs::write(&child, b"old log").unwrap();
    assert!(verify_owner_only(&File::open(&child).unwrap()).is_err());
    create_private_dir(&run).unwrap();
    assert_owner_only(&run, true, true);
    assert_owner_only(&nested, true, false);
    assert_owner_only(&child, false, false);
    assert!(
        verify_private_dir(&root.0).is_err(),
        "existing parent stays broad"
    );
}

#[test]
fn validator_rejects_everyone_read_on_opened_file() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    grant_everyone_read(&path);
    assert_eq!(
        verify_owner_only(&file).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn validator_rejects_null_dacl_on_opened_file() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    apply_sddl(&path, "D:NO_ACCESS_CONTROL", false).unwrap();
    assert_eq!(
        verify_owner_only(&file).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn validator_allows_system_and_administrators() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    apply_sddl(&path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)", false).unwrap();
    verify_owner_only(&file).unwrap();
}

#[test]
fn validator_rejects_foreign_owner_when_token_can_assign_one() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    match apply_sddl(&path, "O:BA", true) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(5 | 1307 | 1314)) => {
            eprintln!("foreign owner fixture requires an elevated token: {error}");
            return;
        }
        Err(error) => panic!("assign foreign owner: {error}"),
    }
    assert_eq!(
        verify_owner_only(&file).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn private_directory_refuses_junction_without_changing_target() {
    let root = TempDir::new();
    let target = root.0.join("target");
    create_private_dir(&target).unwrap();
    let link = root.0.join("junction");
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink /J: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(create_private_dir(&link).is_err());
    assert_owner_only(&target, true, true);
    std::fs::remove_dir(&link).unwrap();
}
