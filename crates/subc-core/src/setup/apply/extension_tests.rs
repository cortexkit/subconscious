use super::*;
use crate::setup::apply::module_steps_tests::inventory;
use cortexkit_test_support::ScratchDir;
use std::io::{Cursor, Write};
use zip::{write::SimpleFileOptions, ZipWriter};

fn archive(files: &[(&str, &str)], link: Option<&str>) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for &(name, bytes) in files {
        zip.start_file(name, options).unwrap();
        zip.write_all(bytes.as_bytes()).unwrap();
    }
    if let Some(link) = link {
        zip.add_symlink(link, "../../operator-file", options)
            .unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn formatted_version(version: u8) -> String {
    format!("{{\"version\":{version}}}")
}

fn archive_for(version: u8) -> Vec<u8> {
    archive(
        &[
            ("manifest.json", &formatted_version(version)),
            ("levels/nested/script.js", "void(0)"),
        ],
        None,
    )
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn install(paths: &Paths, inventory: &mut Inventory, bytes: &[u8]) -> Result<bool, String> {
    ensure_with(paths, inventory, &sha(bytes), |download| {
        fs::write(download, bytes).unwrap();
        Ok(())
    })
}

fn assert_version(paths: &Paths, version: u8) {
    assert_eq!(
        fs::read_to_string(paths.live.join("manifest.json")).unwrap(),
        formatted_version(version)
    );
    assert!(!paths.new.exists());
    assert!(!paths.old.exists());
}

#[test]
fn extension_tree_digest_sorts_path_bytes_and_includes_empty_directories() {
    let root = ScratchDir::new("ck-extension-tree-hash");
    fs::create_dir_all(root.join("z/sub")).unwrap();
    fs::write(root.join("ab"), b"second").unwrap();
    fs::write(root.join("a"), b"first").unwrap();
    fs::write(root.join("z/a"), b"third").unwrap();
    let expected = format!(
        "f {}  a\nf {}  ab\nd  z\nf {}  z/a\nd  z/sub\n",
        sha(b"first"),
        sha(b"second"),
        sha(b"third")
    );
    assert_eq!(
        tree_digest(root.path()).unwrap().unwrap(),
        sha(expected.as_bytes())
    );
}

#[test]
fn extension_rejects_bad_archives_without_leaving_staging_or_changing_owned_rows() {
    let mut duplicate = archive(&[("manifest.json", "{}"), ("manifest.jsom", "{}")], None);
    let bad_name = b"manifest.jsom";
    for index in 0..=duplicate.len() - bad_name.len() {
        if &duplicate[index..index + bad_name.len()] == bad_name {
            duplicate[index + bad_name.len() - 1] = b'n';
        }
    }
    let cases = [
        (
            archive(&[("manifest.json", "{}"), ("../x", "escape")], None),
            "unsafe ZIP entry",
        ),
        (
            archive(&[("manifest.json", "{}"), ("/absolute", "escape")], None),
            "unsafe ZIP entry",
        ),
        (
            archive(&[("manifest.json", "{}")], Some("nested/evil")),
            "not a regular file or directory",
        ),
        (
            archive(&[("nested/manifest.json", "{}")], None),
            "no root manifest.json",
        ),
        (
            archive(&[("manifest.json", "not JSON")], None),
            "manifest.json",
        ),
        (duplicate, "duplicate ZIP entry"),
    ];
    for seeded in [false, true] {
        let root = ScratchDir::new("ck-extension-bad-archives");
        let mut inventory = inventory(root.path());
        let paths = Paths::new(root.path());
        if seeded {
            assert!(install(&paths, &mut inventory, &archive_for(1)).unwrap());
        }
        let before = Row::read(&paths, &inventory).unwrap();
        let original_manifest = fs::read(inventory.manifest()).ok();
        for (bytes, reason) in &cases {
            let error = install(&paths, &mut inventory, bytes).unwrap_err();
            assert!(error.contains(reason), "{error}");
            assert!(!paths.new.exists() && !paths.old.exists());
            assert_eq!(Row::read(&paths, &inventory).unwrap(), before);
            if let Some(original) = &original_manifest {
                assert_eq!(&fs::read(inventory.manifest()).unwrap(), original);
            }
        }
        let error = ensure_with(&paths, &mut inventory, &sha(b"wrong zip"), |destination| {
            fs::write(destination, archive_for(2)).unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("SHA-256 mismatch"), "{error}");
        assert_eq!(Row::read(&paths, &inventory).unwrap(), before);
        if seeded {
            assert_version(&paths, 1);
        } else {
            assert!(!paths.live.exists());
        }
        assert!(!root.join("x").exists());
    }
}

#[test]
fn extension_updates_owned_tree_and_reextracts_a_missing_tree() {
    let root = ScratchDir::new("ck-extension-reinstall");
    let paths = Paths::new(root.path());
    let first = archive_for(1);
    let second = archive_for(2);
    {
        let mut inventory = inventory(root.path());
        assert!(install(&paths, &mut inventory, &first).unwrap());
        assert_version(&paths, 1);
        assert!(install(&paths, &mut inventory, &second).unwrap());
    }
    let mut inventory = inventory(root.path());
    assert_version(&paths, 2);
    let row = Row::read(&paths, &inventory).unwrap();
    assert_eq!(row.zip.as_deref(), Some(sha(&second).as_str()));
    assert_eq!(row.tree, tree_digest(&paths.live).unwrap());
    assert!(row.pending_zip.is_none());
    assert!(!plan(&paths, &inventory, &sha(&second)).unwrap());
    let prior = fs::read(inventory.manifest()).unwrap();
    assert!(
        !ensure_with(&paths, &mut inventory, &sha(&second), |_| panic!(
            "unchanged archive must not download"
        ))
        .unwrap()
    );
    assert_eq!(fs::read(inventory.manifest()).unwrap(), prior);
    fs::remove_dir_all(&paths.live).unwrap();
    assert!(plan(&paths, &inventory, &sha(&second)).unwrap());
    assert!(install(&paths, &mut inventory, &second).unwrap());
    assert_version(&paths, 2);
}

#[test]
fn extension_refuses_unowned_live_tree_and_unexpected_staging_siblings() {
    for index in 0..3 {
        let root = ScratchDir::new("ck-extension-unowned");
        let paths = Paths::new(root.path());
        let path = [&paths.live, &paths.new, &paths.old][index];
        fs::create_dir(path).unwrap();
        fs::write(path.join("operator.txt"), "keep").unwrap();
        let mut inventory = inventory(root.path());
        let zip = archive_for(1);
        for error in [
            plan(&paths, &inventory, &sha(&zip)).unwrap_err(),
            install(&paths, &mut inventory, &zip).unwrap_err(),
        ] {
            assert!(error.contains(&path.display().to_string()), "{error}");
        }
        assert_eq!(
            fs::read_to_string(path.join("operator.txt")).unwrap(),
            "keep"
        );
        assert!(!inventory.owns_path(KIND, &paths.live));
    }
}

#[test]
fn extension_refuses_modified_owned_tree_without_deleting_it() {
    for variant in 0..2 {
        let root = ScratchDir::new("ck-extension-modified");
        let mut inventory = inventory(root.path());
        let paths = Paths::new(root.path());
        let zip = archive_for(1);
        assert!(install(&paths, &mut inventory, &zip).unwrap());
        if variant == 0 {
            fs::create_dir(paths.live.join("levels/nested/empty")).unwrap();
        } else {
            fs::remove_dir_all(&paths.live).unwrap();
            fs::create_dir(&paths.live).unwrap();
            fs::write(paths.live.join("operator.txt"), "keep").unwrap();
        }
        let before = fs::read(inventory.manifest()).unwrap();
        assert!(plan(&paths, &inventory, &sha(&zip))
            .unwrap_err()
            .contains(&paths.live.display().to_string()));
        assert!(install(&paths, &mut inventory, &zip).is_err());
        assert_eq!(fs::read(inventory.manifest()).unwrap(), before);
        assert!(paths.live.is_dir());
    }
}

#[cfg(unix)]
#[test]
fn extension_never_follows_nested_symlink_in_owned_tree() {
    let root = ScratchDir::new("ck-extension-nested-link");
    let mut inventory = inventory(root.path());
    let paths = Paths::new(root.path());
    let zip = archive_for(1);
    assert!(install(&paths, &mut inventory, &zip).unwrap());
    let path = paths.live.join("levels/nested/link");
    std::os::unix::fs::symlink(root.path(), &path).unwrap();
    assert!(plan(&paths, &inventory, &sha(&zip))
        .unwrap_err()
        .contains("levels/nested/link"));
    assert!(install(&paths, &mut inventory, &zip).is_err());
    assert_eq!(fs::read_link(path).unwrap(), root.path());
}

#[cfg(feature = "test-support")]
#[test]
fn child_extension_checkpoint() {
    let Some(root) = std::env::var_os("CK_MODULE_STEP_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let mut inventory = inventory(&root);
    let paths = Paths::new(&root);
    assert!(install(&paths, &mut inventory, &archive_for(2)).unwrap());
}

#[cfg(feature = "test-support")]
#[test]
fn process_restart_restores_pre_swap_and_rename_gap_and_adopts_published_tree() {
    use crate::setup::apply::module_steps_tests::crash_at;
    for (cut, seeded, expected) in [
        ("extension-zip-pending", false, None),
        ("extension-zip-pending", true, Some(1)),
        ("extension-old-renamed", true, Some(1)),
        ("extension-new-published", false, Some(2)),
        ("extension-new-published", true, Some(2)),
    ] {
        let root = ScratchDir::new("ck-extension-process-cut");
        let paths = Paths::new(root.path());
        if seeded {
            assert!(install(&paths, &mut inventory(root.path()), &archive_for(1)).unwrap());
        }
        crash_at(
            root.path(),
            concat!(module_path!(), "::child_extension_checkpoint"),
            cut,
            &[],
        );
        if cut == "extension-zip-pending" {
            fs::create_dir(&paths.new).unwrap();
            fs::write(paths.new.join("partial-file"), b"interrupted extraction").unwrap();
        }
        let mut rows = inventory(root.path());
        let before = Row::read(&paths, &rows).unwrap();
        assert!(before.pending_zip.is_some(), "{cut}");
        assert!(
            plan(&paths, &rows, &sha(&archive_for(expected.unwrap_or(2)))).unwrap(),
            "{cut}"
        );
        if expected == Some(2) {
            assert!(
                ensure_with(&paths, &mut rows, &sha(&archive_for(2)), |_| panic!(
                    "published digest needs no re-extract"
                ))
                .unwrap()
            );
        } else {
            recover(&paths, &mut rows).unwrap();
        }
        let after = Row::read(&paths, &rows).unwrap();
        assert!(
            after.pending_zip.is_none() && after.pending_tree.is_none(),
            "{cut}"
        );
        assert!(!paths.new.exists() && !paths.old.exists(), "{cut}");
        if let Some(version) = expected {
            assert_version(&paths, version);
            assert_eq!(after.tree, tree_digest(&paths.live).unwrap());
            assert_eq!(after.zip, Some(sha(&archive_for(version))));
        } else {
            assert!(!paths.live.exists());
            assert!(!rows.owns_path(KIND, &paths.live));
        }
    }
}
