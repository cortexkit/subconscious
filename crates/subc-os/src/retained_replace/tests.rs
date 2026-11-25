use super::*;
use cortexkit_test_support::ScratchDir;
use std::{fs, io::Write};

fn leaf(name: &str) -> &OsStr {
    OsStr::new(name)
}

#[cfg(unix)]
fn open_parent(path: &Path) -> File {
    File::open(path).unwrap()
}

#[cfg(windows)]
fn open_parent(path: &Path) -> File {
    // The fixture is inside a freshly created scratch directory. This opens
    // that directory, not protected Windows ancestors outside the fixture.
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap()
}

fn stage(parent: &File, publish: bool) -> RetainedTempFile<'_> {
    let mut temp = if publish {
        RetainedTempFile::create_for_publish(parent)
    } else {
        RetainedTempFile::create(parent)
    }
    .unwrap();
    temp.file_mut().write_all(b"replacement").unwrap();
    temp
}

#[test]
fn retained_recovery_moves_between_private_directories_without_replacing() {
    let scratch = ScratchDir::new("retained-recovery-archive");
    let parent = open_parent(&scratch);
    let archive = open_or_create_private_directory_at(&parent, leaf("archive")).unwrap();
    fs::write(scratch.join("proposal"), b"proposal bytes").unwrap();
    fs::write(scratch.join("archive/existing"), b"older recovery").unwrap();
    let mut moved = move_file_aside_nofollow_at(&parent, leaf("proposal"))
        .unwrap()
        .unwrap();
    let collision = moved.relocate(&archive, leaf("existing")).unwrap_err();
    assert_eq!(
        collision.kind(),
        io::ErrorKind::AlreadyExists,
        "{collision:?}"
    );
    moved.relocate(&archive, leaf("saved")).unwrap();
    drop(moved);
    let path = retained_directory_path(&archive).unwrap();
    assert_eq!(fs::read(path.join("saved")).unwrap(), b"proposal bytes");
    assert_eq!(fs::read(path.join("existing")).unwrap(), b"older recovery");
}

#[test]
fn retained_recovery_probe_creates_and_removes_a_staged_file() {
    let scratch = ScratchDir::new("retained-recovery-probe");
    let parent = open_parent(&scratch);
    fs::write(scratch.join("unchanged"), b"settings").unwrap();
    probe_retained_directory_write(&parent).unwrap();
    let names = fs::read_dir(&*scratch)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, [std::ffi::OsString::from("unchanged")]);
    let file = File::open(scratch.join("unchanged")).unwrap();
    assert!(probe_retained_directory_write(&file).is_err());
    assert_eq!(fs::read(scratch.join("unchanged")).unwrap(), b"settings");
}

#[test]
fn retained_recovery_move_uses_the_requested_name_without_replacing() {
    let scratch = ScratchDir::new("retained-named-recovery");
    let parent = open_parent(&scratch);
    fs::write(scratch.join("layer"), b"current").unwrap();
    fs::write(scratch.join(".layer.declined-old"), b"previous").unwrap();
    assert_eq!(
        move_file_to_noreplace_at(&parent, leaf("layer"), leaf(".layer.declined-old"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    let moved = move_file_to_noreplace_at(&parent, leaf("layer"), leaf(".layer.declined-new"))
        .unwrap()
        .unwrap();
    assert_eq!(moved.name(), leaf(".layer.declined-new"));
    drop(moved);
    assert_eq!(
        fs::read(scratch.join(".layer.declined-new")).unwrap(),
        b"current"
    );
    assert_eq!(
        fs::read(scratch.join(".layer.declined-old")).unwrap(),
        b"previous"
    );
}

#[cfg(unix)]
#[test]
fn retained_recovery_mode_uses_a_write_only_sibling() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = ScratchDir::new("retained-recovery-mode");
    let parent = open_parent(&scratch);
    let sample = scratch.join("sibling.jsonc");
    fs::write(&sample, b"{}").unwrap();
    fs::set_permissions(&sample, Permissions::from_mode(0o200)).unwrap();
    let mut replacement = stage(&parent, true);
    replacement
        .cap_settings_creation_mode(&["sibling.jsonc".into()])
        .unwrap();
    replacement
        .publish_restored(leaf("restored.jsonc"), None, None)
        .unwrap();
    assert_eq!(
        fs::metadata(scratch.join("restored.jsonc"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o200
    );
}

#[test]
fn retained_recovery_put_back_never_replaces_a_new_leaf() {
    use std::io::Read;
    let scratch = ScratchDir::new("retained-recovery-collision");
    let parent = open_parent(&scratch);
    let path = scratch.join("layer");
    fs::write(&path, b"moved contents").unwrap();
    let moved = move_file_aside_nofollow_at(&parent, leaf("layer"))
        .unwrap()
        .unwrap();
    let recovery = scratch.join(moved.name());
    let mut verified = Vec::new();
    moved.open().unwrap().read_to_end(&mut verified).unwrap();
    assert_eq!(verified, b"moved contents");
    fs::write(&path, b"new save").unwrap();
    assert!(moved.put_back(leaf("layer")).is_err());
    assert_eq!(fs::read(path).unwrap(), b"new save");
    assert_eq!(fs::read(recovery).unwrap(), b"moved contents");
}

#[test]
fn retained_recovery_discard_deletes_only_inspected_entry() {
    let scratch = ScratchDir::new("retained-recovery-delete");
    let parent = open_parent(&scratch);
    let path = scratch.join("layer");
    assert!(move_file_aside_nofollow_at(&parent, leaf("layer"))
        .unwrap()
        .is_none());
    fs::write(&path, b"reviewed").unwrap();
    let moved = move_file_aside_nofollow_at(&parent, leaf("layer"))
        .unwrap()
        .unwrap();
    let recovery = scratch.join(moved.name());
    let inspected = moved.open().unwrap();
    fs::write(&path, b"different").unwrap();
    moved.discard(&inspected).unwrap();
    drop(inspected);
    assert!(!recovery.exists());
    assert_eq!(fs::read(path).unwrap(), b"different");
}

#[cfg(unix)]
#[test]
fn retained_recovery_ignores_a_link_at_the_old_parent_path() {
    use std::os::unix::fs::symlink;
    let scratch = ScratchDir::new("retained-recovery-link");
    let config = scratch.join("config");
    let original = scratch.join("original");
    let elsewhere = scratch.join("elsewhere");
    for root in [&config, &elsewhere] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("layer"), b"original").unwrap();
    }
    let parent = open_parent(&config);
    fs::rename(&config, &original).unwrap();
    symlink(&elsewhere, &config).unwrap();
    let moved = move_file_aside_nofollow_at(&parent, leaf("layer"))
        .unwrap()
        .unwrap();
    let inspected = moved.open().unwrap();
    stage(&parent, false)
        .publish_restored(leaf("layer"), Some(&inspected), None)
        .unwrap();
    moved.discard(&inspected).unwrap();
    assert_eq!(fs::read(original.join("layer")).unwrap(), b"replacement");
    assert_eq!(fs::read(elsewhere.join("layer")).unwrap(), b"original");
}

#[test]
fn retained_read_distinguishes_absent_directory_hardlink_and_file() {
    use std::io::Read;
    let scratch = ScratchDir::new("retained-read");
    let parent = open_parent(&scratch);
    assert!(open_directory_nofollow_at(&parent, leaf("config"))
        .unwrap()
        .is_none());
    fs::create_dir(scratch.join("config")).unwrap();
    assert!(matches!(
        open_single_link_file_nofollow_at(&parent, leaf("config")),
        Err(OpenRetainedError::NotRegular)
    ));
    let config = open_directory_nofollow_at(&parent, leaf("config"))
        .unwrap()
        .unwrap();
    assert!(open_single_link_file_nofollow_at(&config, leaf("layer"))
        .unwrap()
        .is_none());
    fs::write(scratch.join("config/layer"), b"retained contents").unwrap();
    let mut opened = open_single_link_file_nofollow_at(&config, leaf("layer"))
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    opened.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"retained contents");
    fs::hard_link(scratch.join("config/layer"), scratch.join("other")).unwrap();
    assert!(matches!(
        open_single_link_file_nofollow_at(&config, leaf("layer")),
        Err(OpenRetainedError::Hardlinked)
    ));
    assert!(open_single_link_file_nofollow_at(&config, leaf("../other")).is_err());
}

#[cfg(unix)]
#[test]
fn retained_read_stays_anchored_after_rename_and_rejects_link() {
    use std::{io::Read, os::unix::fs::symlink};
    let scratch = ScratchDir::new("retained-read-rename");
    fs::create_dir(scratch.join("config")).unwrap();
    fs::write(scratch.join("config/layer"), b"old").unwrap();
    let config = open_parent(&scratch.join("config"));
    fs::rename(scratch.join("config"), scratch.join("original")).unwrap();
    fs::create_dir(scratch.join("config")).unwrap();
    fs::write(scratch.join("config/layer"), b"new").unwrap();
    let mut file = open_single_link_file_nofollow_at(&config, leaf("layer"))
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"old");
    symlink(scratch.join("config"), scratch.join("original/link")).unwrap();
    assert!(matches!(
        open_directory_nofollow_at(&config, leaf("link")),
        Err(OpenRetainedError::Symlink)
    ));
}

#[test]
fn no_replace_refuses_existing_file_and_directory() {
    let scratch = ScratchDir::new("retained-no-replace");
    let parent = open_parent(&scratch);
    let stage = stage(&parent, true);
    fs::write(scratch.join("layer"), b"other writer").unwrap();
    let error = stage.publish(leaf("layer")).unwrap_err();
    assert!(!error.renamed());
    assert_eq!(io::Error::from(error).kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(scratch.join("layer")).unwrap(), b"other writer");

    fs::create_dir(scratch.join(".cortexkit")).unwrap();
    let error = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap_err();
    assert!(!error.renamed());
    assert_eq!(io::Error::from(error).kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_dir(&*scratch).unwrap().count(), 2);
}

#[test]
fn checked_replace_refuses_changed_contents_without_publishing() {
    let scratch = ScratchDir::new("retained-checked-replace");
    let parent = open_parent(&scratch);
    let path = scratch.join("layer");
    fs::write(&path, b"old").unwrap();
    let expected = open_single_link_file_nofollow_at(&parent, leaf("layer"))
        .unwrap()
        .unwrap();
    let temp = stage(&parent, false);
    fs::write(&path, b"other writer").unwrap();

    let mut checks = 0;
    let error = temp
        .replace_checked(leaf("layer"), &expected, || {
            checks += 1;
            if fs::read(&path)? == b"old" {
                Ok(())
            } else {
                Err(io::Error::other("target contents changed"))
            }
        })
        .unwrap_err();
    assert!(!error.renamed());
    assert_eq!(
        io::Error::from(error).to_string(),
        "target contents changed"
    );
    assert_eq!(checks, 1);
    assert_eq!(fs::read(&path).unwrap(), b"other writer");
    assert_eq!(fs::read_dir(&*scratch).unwrap().count(), 1);
}

#[test]
fn error_after_rename_reports_publication() {
    let scratch = ScratchDir::new("retained-published-error");
    let parent = open_parent(&scratch);
    let error = stage(&parent, true)
        .publish_impl(leaf("layer"), || {
            Err(io::Error::other("injected flush error"))
        })
        .unwrap_err();
    assert!(error.renamed());
    assert_eq!(fs::read(scratch.join("layer")).unwrap(), b"replacement");
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    #[test]
    fn refuses_symlinked_leaf_without_touching_referent() {
        let scratch = ScratchDir::new("retained-leaf-symlink");
        let parent = File::open(&*scratch).unwrap();
        let original = scratch.join("original");
        fs::write(&original, b"original").unwrap();
        symlink(&original, scratch.join("layer")).unwrap();
        let error = stage(&parent, false)
            .replace(leaf("layer"), &File::open(&original).unwrap())
            .unwrap_err();
        assert!(!error.renamed());
        assert_eq!(io::Error::from(error).raw_os_error(), Some(libc::ELOOP));
        assert!(fs::symlink_metadata(scratch.join("layer"))
            .unwrap()
            .is_symlink());
        assert_eq!(fs::read(original).unwrap(), b"original");
    }

    #[test]
    fn refuses_directory_symlink_and_stays_anchored_after_parent_rename() {
        let scratch = ScratchDir::new("retained-directory-symlink");
        let root = scratch.canonicalize().unwrap();
        let original = root.join("directory");
        let moved = root.join("moved");
        let outside = root.join("outside");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&outside).unwrap();
        let parent = File::open(&original).unwrap();
        fs::rename(&original, &moved).unwrap();
        symlink(&outside, &original).unwrap();
        let root = File::open(root).unwrap();
        let error = create_directory_nofollow_at(&root, leaf("directory")).unwrap_err();
        assert!(!error.renamed());
        assert_eq!(io::Error::from(error).kind(), io::ErrorKind::AlreadyExists);
        let error = stage(&root, true)
            .publish(leaf("directory/layer"))
            .unwrap_err();
        assert!(!error.renamed());
        assert_eq!(io::Error::from(error).kind(), io::ErrorKind::InvalidInput);
        // Stage creation and publication must both use the handle, not the
        // original path which now redirects into outside.
        let config = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap();
        stage(&config, true).publish(leaf("layer")).unwrap();
        assert_eq!(
            fs::read(moved.join(".cortexkit/layer")).unwrap(),
            b"replacement"
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn replaces_atomically_and_preserves_mode_owner_and_group() {
        let scratch = ScratchDir::new("retained-replace-mode");
        let parent = File::open(&*scratch).unwrap();
        let target = scratch.join("layer");
        fs::write(&target, b"original").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let expected = File::open(&target).unwrap();
        let old = expected.metadata().unwrap();
        let mut staging = stage(&parent, false);
        assert_eq!(staging.file_mut().metadata().unwrap().mode() & 0o077, 0);
        staging.replace(leaf("layer"), &expected).unwrap();
        let replacement = fs::metadata(&target).unwrap();
        assert_eq!(replacement.mode() & 0o7777, 0o640);
        assert_eq!(
            (replacement.uid(), replacement.gid()),
            (old.uid(), old.gid())
        );
        assert_ne!(replacement.ino(), old.ino());
        assert_eq!(fs::read(target).unwrap(), b"replacement");
        let mut original = Vec::new();
        std::io::Read::read_to_end(&mut &expected, &mut original).unwrap();
        assert_eq!(original, b"original");
    }

    #[test]
    fn refuses_a_different_regular_file_even_at_the_same_leaf() {
        let scratch = ScratchDir::new("retained-stale-target");
        let parent = File::open(&*scratch).unwrap();
        let target = scratch.join("layer");
        fs::write(&target, b"same bytes").unwrap();
        let expected = File::open(&target).unwrap();
        fs::rename(&target, scratch.join("old")).unwrap();
        fs::write(&target, b"same bytes").unwrap();
        let error = stage(&parent, false)
            .replace(leaf("layer"), &expected)
            .unwrap_err();
        assert!(!error.renamed());
        assert_eq!(fs::read(target).unwrap(), b"same bytes");
    }

    #[test]
    fn new_file_and_directory_have_ordinary_creation_modes() {
        let scratch = ScratchDir::new("retained-publish-mode");
        let parent = File::open(&*scratch).unwrap();
        let ordinary_dir = scratch.join("ordinary-dir");
        let ordinary_file = scratch.join("ordinary-file");
        fs::create_dir(&ordinary_dir).unwrap();
        File::create(&ordinary_file).unwrap();
        let published_dir = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap();
        stage(&parent, true).publish(leaf("layer")).unwrap();
        assert_eq!(
            published_dir.metadata().unwrap().mode(),
            fs::metadata(ordinary_dir).unwrap().mode()
        );
        assert_eq!(
            fs::metadata(scratch.join("layer")).unwrap().mode(),
            fs::metadata(ordinary_file).unwrap().mode()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn preserves_posix_acl_and_inherits_it_on_publish() {
        use rustix::fs::{fgetxattr, fsetxattr, XattrFlags};

        let scratch = ScratchDir::new("retained-posix-acl");
        let parent = File::open(&*scratch).unwrap();
        let acl_name = "system.posix_acl_access";
        // Linux xattr ACL v2: grant one named user access in addition to the
        // ordinary user/group/other entries. The mask matches mode 0640.
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permission, id) in [
            (1u16, 6u16, u32::MAX),
            (2, 4, 65534),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permission.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        let target = scratch.join("layer");
        fs::write(&target, b"original").unwrap();
        let expected = File::open(&target).unwrap();
        fsetxattr(&expected, acl_name, &acl, XattrFlags::empty()).unwrap();
        stage(&parent, false)
            .replace(leaf("layer"), &expected)
            .unwrap();
        let mut default = acl.clone();
        default[6] |= 1; // The owner also needs search permission on new directories.
        fsetxattr(
            &parent,
            "system.posix_acl_default",
            &default,
            XattrFlags::empty(),
        )
        .unwrap();
        let config = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap();
        stage(&config, true).publish(leaf("new-layer")).unwrap();
        for file in [target, scratch.join(".cortexkit/new-layer")] {
            let mut buffer = [0; 64];
            let count = fgetxattr(File::open(file).unwrap(), acl_name, &mut buffer[..]).unwrap();
            assert_eq!(&buffer[..count], acl);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn preserves_extended_acl() {
        use std::process::Command;

        let scratch = ScratchDir::new("retained-mac-acl");
        let parent = File::open(&*scratch).unwrap();
        let target = scratch.join("layer");
        fs::write(&target, b"original").unwrap();
        assert!(Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&target)
            .status()
            .unwrap()
            .success());
        let acl = || {
            let listing = Command::new("/bin/ls")
                .arg("-le")
                .arg(&target)
                .output()
                .unwrap();
            assert!(listing.status.success());
            String::from_utf8(listing.stdout)
                .unwrap()
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let original = acl();
        assert!(original.contains("everyone allow read"));
        stage(&parent, false)
            .replace(leaf("layer"), &File::open(&target).unwrap())
            .unwrap();
        assert_eq!(acl(), original);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keeps_new_file_acl_inheritance_out_of_private_staging() {
        use std::process::Command;

        let scratch = ScratchDir::new("retained-mac-inherited-acl");
        let parent = File::open(&*scratch).unwrap();
        let target = scratch.join("without-acl");
        fs::write(&target, b"original").unwrap();
        assert!(Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,file_inherit,directory_inherit"])
            .arg(&*scratch)
            .status()
            .unwrap()
            .success());
        let get_acl = |path: &Path| {
            let listing = Command::new("/bin/ls")
                .arg("-led")
                .arg(path)
                .output()
                .unwrap();
            assert!(listing.status.success());
            let listing = String::from_utf8(listing.stdout).unwrap();
            listing
                .lines()
                .any(|line| line.contains("everyone") && line.contains("allow read"))
        };
        let stage_file = stage(&parent, false);
        assert!(!get_acl(&scratch.join(&stage_file.stage.name)));
        stage_file
            .replace(leaf("without-acl"), &File::open(&target).unwrap())
            .unwrap();
        assert!(!get_acl(&target));
        let config = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap();
        stage(&config, true).publish(leaf("new-layer")).unwrap();
        assert!(get_acl(&scratch.join(".cortexkit/new-layer")));
    }
}

#[cfg(windows)]
#[test]
fn publishes_into_a_new_retained_directory_on_windows() {
    let scratch = ScratchDir::new("retained-windows-publish");
    let parent = open_parent(&scratch);
    let config = create_directory_nofollow_at(&parent, leaf(".cortexkit")).unwrap();
    stage(&config, true).publish(leaf("layer")).unwrap();
    let layer = scratch.join(".cortexkit/layer");
    assert_eq!(fs::read(&layer).unwrap(), b"replacement");
    let expected = File::open(&layer).unwrap();
    let mut replacement = RetainedTempFile::create(&config).unwrap();
    replacement.file_mut().write_all(b"new contents").unwrap();
    replacement.replace(leaf("layer"), &expected).unwrap();
    assert_eq!(fs::read(layer).unwrap(), b"new contents");
}

#[cfg(windows)]
#[test]
fn retained_read_and_replace_refuse_reparse_parent_and_leaf() {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt, process::Command};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let scratch = ScratchDir::new("retained-reparse");
    let target = scratch.join("target");
    let junction = scratch.join("junction");
    fs::create_dir(&target).unwrap();
    assert!(Command::new("cmd")
        .args(["/d", "/c", "mklink", "/j"])
        .arg(&junction)
        .arg(&target)
        .status()
        .unwrap()
        .success());
    let reparse_handle = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&junction)
        .unwrap();
    assert_eq!(
        RetainedTempFile::create(&reparse_handle)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );

    let parent = open_parent(&scratch);
    assert!(matches!(
        open_directory_nofollow_at(&parent, leaf("junction")),
        Err(OpenRetainedError::Symlink)
    ));
    assert!(matches!(
        open_single_link_file_nofollow_at(&parent, leaf("junction")),
        Err(OpenRetainedError::Symlink)
    ));
    fs::write(scratch.join("expected"), b"original").unwrap();
    let expected = File::open(scratch.join("expected")).unwrap();
    let error = stage(&parent, false)
        .replace(leaf("junction"), &expected)
        .unwrap_err();
    assert!(!error.renamed());
    assert_eq!(io::Error::from(error).kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}
