use super::{test_support::*, *};
use cortexkit_test_support::ScratchDir;
use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

struct TempDir(ScratchDir);

impl TempDir {
    fn new() -> Self {
        Self(ScratchDir::new("subc-acl"))
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
    // A plain write takes the token's default owner (Administrators on an
    // elevated runner); what matters here is the access list it inherits.
    std::fs::write(&child, b"private").unwrap();
    assert_dacl_owner_only(&child, false, false);
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
    // `run` existed before narrowing, so only its access list changes; an
    // elevated test runner owns it as Administrators.
    assert_dacl_owner_only(&run, true, true);
    assert_dacl_owner_only(&nested, true, false);
    assert_dacl_owner_only(&child, false, false);
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

/// An elevated process creates files owned by Administrators. Administrators can
/// bypass file security anyway, so such an owner is accepted, like SYSTEM.
#[test]
fn validator_accepts_administrators_owner_when_token_can_assign_one() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    match apply_sddl(&path, "O:BA", true) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(5 | 1300 | 1307 | 1314)) => {
            eprintln!("owner fixture requires an elevated token: {error}");
            return;
        }
        Err(error) => panic!("assign Administrators owner: {error}"),
    }
    verify_owner_only(&file).unwrap();
}

/// BUILTIN\Users is an ordinary group: a file it owns could be re-permissioned by
/// any local user, so it must be refused.
#[test]
fn validator_rejects_foreign_owner_when_token_can_assign_one() {
    let root = TempDir::new();
    let path = root.0.join("key.json");
    let file = create_private_file(&path).unwrap();
    match apply_sddl(&path, "O:BU", true) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(5 | 1300 | 1307 | 1314)) => {
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

// Scratch roots can already be verbatim. Remove that prefix in these fixtures
// so a raw Win32 call cannot accidentally inherit long-path support from setup.
fn ordinary_path(path: &Path) -> PathBuf {
    let value: Vec<u16> = path.as_os_str().encode_wide().collect();
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let value = if value.starts_with(&unc) {
        r"\\"
            .encode_utf16()
            .chain(value.into_iter().skip(unc.len()))
            .collect::<Vec<_>>()
    } else if value.starts_with(&verbatim) {
        value[verbatim.len()..].to_vec()
    } else {
        value
    };
    PathBuf::from(OsString::from_wide(&value))
}

fn long_parent(root: &Path) -> PathBuf {
    let mut path = ordinary_path(root);
    while path.as_os_str().encode_wide().count() <= 300 {
        path.push("long-path-component");
    }
    // Use std's long-path support for setup so removing the ACL helper's
    // conversion fails at private creation, not while building the fixture.
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn private_file_beyond_max_path_has_owner_only_acl() {
    let root = TempDir::new();
    let path = long_parent(&root.0).join("connection.json");
    assert!(path.as_os_str().encode_wide().count() > 300);
    let file = create_private_file(&path).expect("create private file beyond MAX_PATH");
    assert_owner_only(&path, false, true);
    verify_owner_only(&file).unwrap();
}

#[test]
fn private_directory_beyond_max_path_has_owner_only_acl() {
    let root = TempDir::new();
    let parent = long_parent(&root.0).join("private");
    let path = parent.join("run");
    assert!(path.as_os_str().encode_wide().count() > 300);
    create_private_dir(&path).expect("create private directory beyond MAX_PATH");
    assert_owner_only(&parent, true, true);
    assert_owner_only(&path, true, true);
    verify_private_dir(&path).unwrap();
    create_private_dir(&path).expect("tighten existing private directory beyond MAX_PATH");
    assert_owner_only(&path, true, true);
}

#[test]
fn forward_slashes_and_parent_components_resolve_before_verbatim_prefix() {
    let root = TempDir::new();
    let base = ordinary_path(&root.0);
    let supplied = PathBuf::from(format!(
        "{}/absent/../run",
        base.to_str().unwrap().replace('\\', "/")
    ));
    create_private_dir(&supplied).unwrap();
    let expected = base.join("run");
    assert_owner_only(&expected, true, true);
    verify_private_dir(&supplied).unwrap();
    let file_path = supplied.join("absent/../connection.json");
    let file = create_private_file(&file_path).unwrap();
    assert_owner_only(&expected.join("connection.json"), false, true);
    verify_owner_only(&file).unwrap();
    assert!(!base.join("absent").exists());
    assert!(!expected.join("absent").exists());
}

#[test]
fn wide_drive_path_is_normalized_and_verbatim() {
    assert_eq!(
        wide(Path::new(r"C:/folder/./absent/../file")).unwrap(),
        r"\\?\C:\folder\file"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>()
    );
}

#[test]
fn wide_unc_path_is_normalized_and_verbatim() {
    assert_eq!(
        wide(Path::new(r"\\server\share\folder\..\file")).unwrap(),
        r"\\?\UNC\server\share\file"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>()
    );
}

#[test]
fn wide_relative_path_is_resolved_and_normalized() {
    let expected = std::env::current_dir().unwrap().join("connection.json");
    let expected = ordinary_path(&expected);
    let expected = expected.to_str().unwrap();
    let expected = match expected.strip_prefix(r"\\") {
        Some(unc) => format!(r"\\?\UNC\{unc}"),
        None => format!(r"\\?\{expected}"),
    };
    assert_eq!(
        wide(Path::new("absent/../connection.json")).unwrap(),
        expected.encode_utf16().chain(Some(0)).collect::<Vec<_>>()
    );
}

#[test]
fn wide_verbatim_paths_are_not_double_prefixed_or_normalized() {
    for path in [
        r"\\?\C:\folder\..\file",
        r"\\?\UNC\server\share\folder\..\file",
    ] {
        assert_eq!(
            wide(Path::new(path)).unwrap(),
            path.encode_utf16().chain(Some(0)).collect::<Vec<_>>()
        );
    }
    let root = TempDir::new();
    let path = root.0.join("verbatim.json");
    let verbatim = root.0.canonicalize().unwrap().join("verbatim.json");
    assert!(verbatim.to_str().unwrap().starts_with(r"\\?\"));
    let file = create_private_file(&verbatim).unwrap();
    assert_owner_only(&path, false, true);
    verify_owner_only(&file).unwrap();
}

#[test]
fn wide_rejects_embedded_nul() {
    assert_eq!(
        wide(Path::new("file\0name")).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}
