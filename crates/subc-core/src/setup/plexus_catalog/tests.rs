use super::*;
use crate::setup::{
    install_journal::system::SystemRecovery,
    runtime::{self, RuntimePlatform, SystemCommandRunner},
};
use cortexkit_test_support::ScratchDir;
use std::{
    io::{Cursor, Write},
    time::Duration,
};
use zip::{write::SimpleFileOptions, ZipWriter};

fn archive(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn install(path: &Path, inventory: &mut Inventory, bytes: &[u8]) -> String {
    let digest = format!("{:x}", Sha256::digest(bytes));
    ensure_with(path, inventory, &digest, |to| {
        fs::write(to, bytes).map_err(|error| error.to_string())
    })
    .unwrap();
    digest
}

#[test]
fn plexus_catalog_replaces_removed_files_and_rolls_back_with_programs() {
    let scratch = ScratchDir::new("plexus-catalog");
    let data = fs::canonicalize(scratch.path()).unwrap();
    let path = path(&data);
    let mut inventory = Inventory::load_locked(
        data.join("installer-manifest.json"),
        "linux-x64",
        "test",
        Duration::ZERO,
    )
    .unwrap();
    let platform = RuntimePlatform::current();
    let mut recovery = SystemRecovery {
        executable: &data.join("ck"),
        connection: None,
        platform,
        paths: &runtime::runtime_paths(platform, &data, &data),
        runner: &mut SystemCommandRunner,
    };
    let old_zip = archive(&[("removed.jsonc", "removed"), ("updated.jsonc", "old")]);
    let new_zip = archive(&[("updated.jsonc", "new"), ("added.jsonc", "added")]);
    inventory.start_transaction().unwrap();
    let old_digest = install(&path, &mut inventory, &old_zip);
    inventory.commit_transaction(&mut recovery).unwrap();
    ensure_with(&path, &mut inventory, &old_digest, |_| {
        panic!("current catalog should not download")
    })
    .unwrap();

    let binary = data.join("ck-plexus");
    let next = data.join("new-plexus");
    fs::write(&binary, "old program").unwrap();
    fs::write(&next, "new program").unwrap();
    inventory.start_transaction().unwrap();
    inventory
        .transaction()
        .unwrap()
        .replace(&binary, &next)
        .unwrap();
    install(&path, &mut inventory, &new_zip);
    assert!(!path.join("removed.jsonc").exists());
    assert_eq!(
        fs::read_to_string(path.join("updated.jsonc")).unwrap(),
        "new"
    );
    inventory.undo_transaction(&mut recovery).unwrap();
    assert_eq!(fs::read_to_string(&binary).unwrap(), "old program");
    assert_eq!(
        fs::read_to_string(path.join("removed.jsonc")).unwrap(),
        "removed"
    );
    assert_eq!(
        fs::read_to_string(path.join("updated.jsonc")).unwrap(),
        "old"
    );
    assert!(!path.join("added.jsonc").exists());
    assert!(!needs_refresh(&path, &inventory, &old_digest).unwrap());

    inventory.start_transaction().unwrap();
    let new_digest = install(&path, &mut inventory, &new_zip);
    inventory.commit_transaction(&mut recovery).unwrap();
    assert!(!path.join("removed.jsonc").exists());
    assert_eq!(
        fs::read_to_string(path.join("added.jsonc")).unwrap(),
        "added"
    );
    assert!(!needs_refresh(&path, &inventory, &new_digest).unwrap());
}

#[test]
fn plexus_catalog_checks_digest_before_extraction_and_rejects_non_root_non_files() {
    let scratch = ScratchDir::new("invalid-plexus-catalog");
    let root = scratch.path();
    let mut inventory = Inventory::load_snapshot(root.join("manifest.json"), "linux-x64").unwrap();
    let catalog = path(root);
    let error = ensure_with(&catalog, &mut inventory, &"01".repeat(32), |to| {
        fs::write(to, b"not a zip").map_err(|error| error.to_string())
    })
    .unwrap_err();
    assert!(error.contains("SHA-256 mismatch"), "{error}");

    let mut link = ZipWriter::new(Cursor::new(Vec::new()));
    link.add_symlink("link.jsonc", "good.jsonc", SimpleFileOptions::default())
        .unwrap();
    let link = link.finish().unwrap().into_inner();
    for bad in [
        archive(&[("nested/a.jsonc", "bad")]),
        archive(&[("../a.jsonc", "bad")]),
        archive(&[("dir.jsonc/", "")]),
        archive(&[("not-json.txt", "bad")]),
        link,
    ] {
        let digest = format!("{:x}", Sha256::digest(&bad));
        let error = ensure_with(&catalog, &mut inventory, &digest, |file| {
            fs::write(file, &bad).map_err(|error| error.to_string())
        })
        .unwrap_err();
        assert!(
            error.contains("expected a plain root .jsonc file"),
            "{error}"
        );
        assert!(!catalog.exists());
    }
}
