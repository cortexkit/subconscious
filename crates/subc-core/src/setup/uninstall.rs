use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    apply::extension,
    components::digest_file,
    install_journal,
    inventory::Inventory,
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform},
};

const CHROME_MANIFEST_NOTE: &str = "Chrome native-messaging manifest left in place: ~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.cortexkit.cerebellum.json";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UninstallReport {
    pub removed: Vec<PathBuf>,
    pub retained: Vec<String>,
}

/// Remove only paths named by the ownership inventory. Configuration and stores
/// are deliberately retention-only categories, even when setup created them.
pub fn uninstall<R: CommandRunner>(
    platform: RuntimePlatform,
    runtime_paths: &RuntimePaths,
    daemon_pid: Option<u32>,
    runner: &mut R,
    inventory: &mut Inventory,
    config_path: &Path,
    store_paths: &[PathBuf],
) -> Result<UninstallReport, String> {
    let mut report = uninstall_user_data(platform, inventory, config_path, store_paths)?;
    if inventory.owns_path("runtime-registration", &runtime_paths.definition) {
        if let Some(transaction) = inventory.transaction() {
            let mut service = super::install_journal::registration::Service {
                platform,
                paths: runtime_paths,
                runner,
            };
            let path = &runtime_paths.definition;
            let registered = runtime::observe(platform, service.runner)?.registered;
            let before = service.record(
                path,
                registered,
                super::install_journal::files::digest(path)?,
            );
            let after = service.record(path, false, None);
            if before.registered {
                let journal = transaction.journal();
                let index = service.prepare(journal, before, after)?;
                service.replay(journal.step(index))?;
                journal.done(index)?;
            }
        } else {
            runtime::deregister(platform, runtime_paths, daemon_pid, runner)?;
        }
        inventory.remove_owned_path("runtime-registration", &runtime_paths.definition);
    }
    remove_owned_path(
        "runtime-definition",
        &runtime_paths.definition,
        inventory,
        &mut report,
    )?;

    for kind in ["managed-link", "managed-binary", "binary-placement"] {
        for path in inventory.paths_for_kind(kind) {
            #[cfg(unix)]
            if platform == RuntimePlatform::Macos {
                if let Some(bundle) = super::daemon_bundle::containing_bundle(&path) {
                    if inventory.transaction_is_open() && path.is_file() {
                        if let Some(expected) = inventory
                            .entry_for_path(kind, &path)
                            .and_then(|row| row.get("sha256"))
                            .and_then(serde_json::Value::as_str)
                        {
                            if digest_file(&path)? != expected {
                                report.retained.push(format!(
                                    "modified managed destination retained: {}",
                                    bundle.display()
                                ));
                                continue;
                            }
                        }
                        super::install_journal::bundle::remove(
                            inventory.transaction().unwrap().journal(),
                            bundle,
                            runner,
                        )?;
                        inventory.remove_owned_path(kind, &path);
                        report.removed.push(bundle.to_owned());
                        continue;
                    }
                }
            }
            remove_owned_path(kind, &path, inventory, &mut report)?;
        }
    }

    Ok(report)
}

pub(super) fn uninstall_user_data(
    platform: RuntimePlatform,
    inventory: &mut Inventory,
    config_path: &Path,
    store_paths: &[PathBuf],
) -> Result<UninstallReport, String> {
    let mut report = UninstallReport::default();
    report
        .retained
        .push(format!("configuration: {}", config_path.display()));
    for store in store_paths {
        report.retained.push(format!("store: {}", store.display()));
    }
    uninstall_extension(inventory, &mut report)?;
    if platform == RuntimePlatform::Macos {
        report.retained.push(CHROME_MANIFEST_NOTE.to_string());
    }
    Ok(report)
}

fn extension_paths(inventory: &Inventory) -> extension::Paths {
    let data_dir = inventory
        .manifest()
        .parent()
        .expect("inventory has a parent");
    extension::Paths::new(data_dir)
}

/// Report paths left by an uninstall that found no managed destinations.
pub(super) fn unmanaged_uninstall_notes(
    platform: RuntimePlatform,
    inventory: &Inventory,
) -> Vec<String> {
    let paths = extension_paths(inventory);
    [&paths.live, &paths.new, &paths.old]
        .into_iter()
        .filter_map(|path| extension_retention_note(path))
        .chain((platform == RuntimePlatform::Macos).then(|| CHROME_MANIFEST_NOTE.to_string()))
        .collect()
}

fn uninstall_extension(
    inventory: &mut Inventory,
    report: &mut UninstallReport,
) -> Result<(), String> {
    let paths = extension_paths(inventory);
    if inventory
        .entry_for_path(extension::KIND, &paths.live)
        .is_some_and(|row| row.get("pending_zip_sha256").is_some())
    {
        if let Err(reason) = extension::recover(&paths, inventory) {
            report
                .retained
                .push(format!("Chrome extension swap not recovered: {reason}"));
            report.retained.extend(
                [&paths.live, &paths.new, &paths.old]
                    .into_iter()
                    .filter_map(|path| extension_retention_note(path)),
            );
            return Ok(());
        }
    }

    report.retained.extend(
        [&paths.new, &paths.old]
            .into_iter()
            .filter_map(|path| extension_retention_note(path)),
    );
    let row = inventory.entry_for_path(extension::KIND, &paths.live);
    match extension::tree_digest(&paths.live) {
        Ok(None) => {}
        Ok(Some(digest))
            if row.and_then(|row| row.get("tree_digest").and_then(serde_json::Value::as_str))
                == Some(digest.as_str()) =>
        {
            fs::remove_dir_all(&paths.live).map_err(|error| {
                format!(
                    "could not remove Chrome extension {}: {error}",
                    paths.live.display()
                )
            })?;
            install_journal::sync_parent(&paths.live)?;
            report.removed.push(paths.live.clone());
        }
        Ok(Some(_)) => {
            report
                .retained
                .extend(extension_retention_note(&paths.live));
            return Ok(());
        }
        Err(reason) => {
            report.retained.push(format!(
                "Chrome extension left in place: {} ({reason})",
                paths.live.display()
            ));
            return Ok(());
        }
    }
    if row.is_some() {
        inventory.drop_retained(extension::KIND, &paths.live)?;
    }
    Ok(())
}

fn extension_retention_note(path: &Path) -> Option<String> {
    (!fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound))
        .then(|| format!("Chrome extension path left in place: {}", path.display()))
}

fn remove_owned_path(
    kind: &str,
    path: &Path,
    inventory: &mut Inventory,
    report: &mut UninstallReport,
) -> Result<(), String> {
    if !inventory.owns_path(kind, path) {
        return Ok(());
    }
    if let Some(targets) = inventory
        .entry_for_path(kind, path)
        .and_then(super::ckni::recorded_symlink_targets)
    {
        return remove_owned_symlink(kind, path, &targets, inventory, report);
    }
    if path.exists() {
        if let Some(expected) = inventory
            .entry_for_path(kind, path)
            .and_then(|entry| entry.get("sha256"))
            .and_then(|digest| digest.as_str())
        {
            let actual = digest_file(path)?;
            if actual != expected {
                report.retained.push(format!(
                    "modified managed destination retained: {}",
                    path.display()
                ));
                return Ok(());
            }
        }
        if path.is_dir() {
            return Err(format!(
                "refusal: inventory-owned {} is a directory and will not be removed: {}",
                kind,
                path.display()
            ));
        }
        if let Some(transaction) = inventory.transaction() {
            transaction.remove(path)?;
            report.removed.push(path.to_owned());
            inventory.remove_owned_path(kind, path);
            return Ok(());
        }
        fs::remove_file(path).map_err(|error| {
            format!(
                "could not remove inventory-owned {}: {error}",
                path.display()
            )
        })?;
        report.removed.push(path.to_path_buf());
    }
    inventory.remove_owned_path(kind, path);
    Ok(())
}

/// Removes a symlink setup recorded, judging the link itself rather than
/// the file it points at: a row for a symlink carries no digest, so the
/// link's own target is the only evidence it is still setup's. A link that
/// still points where setup pointed it is removed even when that target is
/// already gone. Anything else at the path (a regular file, or a link
/// pointing elsewhere) was put there by someone else and is kept, reported
/// the way a modified copy is.
fn remove_owned_symlink(
    kind: &str,
    path: &Path,
    targets: &[PathBuf],
    inventory: &mut Inventory,
    report: &mut UninstallReport,
) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect inventory-owned {}: {error}",
                path.display()
            ))
        }
        Ok(metadata) => {
            let ours = if !metadata.file_type().is_symlink() {
                false
            } else {
                fs::read_link(path).is_ok_and(|target| targets.contains(&target))
            };
            if !ours {
                report.retained.push(format!(
                    "modified managed destination retained: {}",
                    path.display()
                ));
                return Ok(());
            }
            #[cfg(unix)]
            if let Some(transaction) = inventory.transaction() {
                super::install_journal::symlink::place(transaction.journal(), path, None)?;
                report.removed.push(path.to_owned());
                inventory.remove_owned_path(kind, path);
                return Ok(());
            }
            fs::remove_file(path).map_err(|error| {
                format!(
                    "could not remove inventory-owned {}: {error}",
                    path.display()
                )
            })?;
            report.removed.push(path.to_path_buf());
        }
    }
    inventory.remove_owned_path(kind, path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[derive(Default)]
    struct SuccessfulRunner;
    impl CommandRunner for SuccessfulRunner {
        fn run(
            &mut self,
            _program: &str,
            _args: &[String],
        ) -> Result<super::super::runtime::CommandResult, String> {
            Ok(super::super::runtime::CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn fixture_dir(name: &str) -> ScratchDir {
        ScratchDir::new(name)
    }

    fn seed_extension(path: &Path, contents: &str) -> String {
        fs::create_dir(path).unwrap();
        fs::write(path.join("manifest.json"), contents).unwrap();
        extension::tree_digest(path).unwrap().unwrap()
    }

    fn mac_inventory(manifest: &Path) -> Inventory {
        Inventory::load_locked(
            manifest,
            "darwin-arm64",
            "test uninstall",
            std::time::Duration::ZERO,
        )
        .unwrap()
    }

    #[test]
    fn uninstall_recovers_both_swap_rename_cuts_and_keeps_user_data() {
        for published in [false, true] {
            let root = fixture_dir(if published {
                "uninstall-published"
            } else {
                "uninstall-rename-gap"
            });
            let manifest = root.join("installer-manifest.json");
            let paths = extension::Paths::new(&root);
            // Both interruption points occur after renaming live to .old.
            // Before the second rename, only .new has the pending digest;
            // after it, live does.
            let old = seed_extension(&paths.old, "old tree");
            let pending = seed_extension(
                if published { &paths.live } else { &paths.new },
                "pending tree",
            );
            let mut seed = mac_inventory(&manifest);
            seed.persist_retained(
                extension::KIND,
                &paths.live,
                Map::from_iter([
                    ("zip_sha256".into(), "11".repeat(32).into()),
                    ("tree_digest".into(), old.into()),
                    ("pending_zip_sha256".into(), "22".repeat(32).into()),
                    ("pending_tree_digest".into(), pending.into()),
                ]),
            )
            .unwrap();

            let broca = root.join("broca.jsonc");
            let fusiform = root.join("fusiform.jsonc");
            let simulated_vault = root.join("vault");
            let preserved = [
                (&broca, "broca settings"),
                (&fusiform, "fusiform settings"),
                (&simulated_vault, "vault records and grants"),
            ];
            for (path, bytes) in preserved {
                fs::write(path, bytes).unwrap();
            }
            seed.persist_retained(
                "broca-key-intent",
                &broca,
                Map::from_iter([("deployment_key_id".into(), "42".into())]),
            )
            .unwrap();
            seed.record("settings-receipt", &fusiform, Map::new());
            seed.save().unwrap();
            drop(seed);

            let mut inventory = mac_inventory(&manifest);
            let runtime_paths = RuntimePaths {
                definition: root.join("unit"),
                daemon: root.join("ck-subc"),
                config_store_root: root.join("config-store"),
            };
            let report = uninstall(
                RuntimePlatform::Macos,
                &runtime_paths,
                None,
                &mut SuccessfulRunner,
                &mut inventory,
                &root.join("subc.jsonc"),
                &[],
            )
            .unwrap();
            assert_eq!(report.removed.as_slice(), std::slice::from_ref(&paths.live));
            assert!(report
                .retained
                .iter()
                .any(|line| line == CHROME_MANIFEST_NOTE));
            assert!(!paths.live.exists());
            assert!(!paths.new.exists());
            assert!(!paths.old.exists());
            for (path, bytes) in preserved {
                assert_eq!(fs::read_to_string(path).unwrap(), bytes);
            }
            let disk = Inventory::load_snapshot(&manifest, "darwin-arm64").unwrap();
            assert!(!disk.owns_path(extension::KIND, &paths.live));
            assert!(disk.owns_path("broca-key-intent", &broca));
            assert!(disk.owns_path("settings-receipt", &fusiform));
        }
    }

    #[test]
    fn uninstall_keeps_unverifiable_extension_paths_but_drops_an_absent_live_row() {
        let root = fixture_dir("uninstall-unverifiable-extension");
        let mut inventory = mac_inventory(&root.join("manifest.json"));
        let paths = extension::Paths::new(&root);
        let digest = seed_extension(&paths.live, "original");
        assert!(
            unmanaged_uninstall_notes(RuntimePlatform::Macos, &inventory)
                .iter()
                .any(|line| line.contains(paths.live.to_str().unwrap()))
        );
        let report = uninstall_user_data(
            RuntimePlatform::Macos,
            &mut inventory,
            &root.join("config"),
            &[],
        )
        .unwrap();
        assert!(
            report.removed.is_empty(),
            "an unowned tree must not be removed"
        );
        let kept = |p: &Path, report: &UninstallReport| {
            report.retained.contains(&format!(
                "Chrome extension path left in place: {}",
                p.display()
            ))
        };
        assert!(kept(&paths.live, &report));

        inventory
            .persist_retained(
                extension::KIND,
                &paths.live,
                Map::from_iter([
                    ("zip_sha256".into(), "11".repeat(32).into()),
                    ("tree_digest".into(), digest.into()),
                ]),
            )
            .unwrap();
        fs::create_dir(paths.live.join("empty-new-directory")).unwrap();
        seed_extension(&paths.new, "user's staging sibling");
        seed_extension(&paths.old, "user's backup sibling");
        let report = uninstall_user_data(
            RuntimePlatform::Macos,
            &mut inventory,
            &root.join("config"),
            &[],
        )
        .unwrap();
        assert!(report.removed.is_empty());
        for path in [&paths.live, &paths.new, &paths.old] {
            assert!(
                path.is_dir() && kept(path, &report),
                "{} should be kept",
                path.display()
            );
        }
        assert!(inventory.owns_path(extension::KIND, &paths.live));

        fs::remove_dir_all(&paths.live).unwrap();
        let report = uninstall_user_data(
            RuntimePlatform::Macos,
            &mut inventory,
            &root.join("config"),
            &[],
        )
        .unwrap();
        assert!(report.removed.is_empty());
        assert!(!inventory.owns_path(extension::KIND, &paths.live));
        assert!(paths.new.exists() && paths.old.exists());
    }

    #[test]
    fn uninstall_removes_only_inventory_paths_and_retains_configuration_and_stores() {
        let root = fixture_dir("owned-only");
        let managed = root.join("ck-subc");
        let unrelated = root.join("notes.txt");
        let config = root.join("subc.jsonc");
        let store = root.join("store.sqlite");
        fs::write(&managed, "managed").expect("managed binary");
        fs::write(&unrelated, "user data").expect("user data");
        fs::write(&config, "user config").expect("config");
        fs::write(&store, "user store").expect("store");
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: managed.clone(),
            config_store_root: root.join("config-store"),
        };
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert(
            "sha256".to_string(),
            serde_json::Value::String(digest_file(&managed).expect("digest")),
        );
        inventory.record("managed-binary", &managed, fields);

        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            None,
            &mut SuccessfulRunner,
            &mut inventory,
            &config,
            std::slice::from_ref(&store),
        )
        .expect("uninstall");
        assert!(!managed.exists());
        assert!(unrelated.exists());
        assert!(config.exists());
        assert!(store.exists());
        assert!(report
            .retained
            .iter()
            .any(|line| line.contains("configuration")));
        assert!(report.retained.iter().any(|line| line.contains("store")));
    }

    #[test]
    fn uninstall_retains_modified_bytes_even_when_archive_digest_matches() {
        let root = fixture_dir("archive-digest-is-not-ownership");
        let managed = root.join("ck-subc");
        fs::write(&managed, "placed-bytes").expect("managed binary");
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: managed.clone(),
            config_store_root: root.join("config-store"),
        };
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let on_disk = digest_file(&managed).expect("on-disk digest");
        let mut fields = Map::new();
        fields.insert(
            "sha256".to_string(),
            serde_json::Value::String("ff".repeat(32)),
        );
        fields.insert(
            "archive_sha256".to_string(),
            serde_json::Value::String(on_disk),
        );
        inventory.record("managed-binary", &managed, fields);

        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            None,
            &mut SuccessfulRunner,
            &mut inventory,
            &root.join("subc.jsonc"),
            &[],
        )
        .expect("uninstall");

        assert!(
            managed.exists(),
            "ownership uses the extracted-binary digest; a matching archive digest must not delete changed bytes"
        );
        assert!(report
            .retained
            .iter()
            .any(|line| line.contains("modified managed destination retained")));
    }

    /// A `ck` and a `ckni` symlink row as setup records them, with whatever
    /// `place` puts at the `ckni` path.
    #[cfg(unix)]
    fn uninstall_ckni(
        name: &str,
        place: impl FnOnce(&Path, &Path),
    ) -> (ScratchDir, PathBuf, UninstallReport, Inventory) {
        use super::super::ckni::CKNI_KIND;
        let root = fixture_dir(name);
        let ck = root.join("ck");
        let ckni = root.join("ckni");
        place(&ck, &ckni);
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert(
            "mechanism".into(),
            serde_json::Value::String("symlink".into()),
        );
        fields.insert(
            "links_to".into(),
            serde_json::Value::String(ck.to_string_lossy().into_owned()),
        );
        inventory.record(CKNI_KIND, &ckni, fields);
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: root.join("ck-subc"),
            config_store_root: root.join("config-store"),
        };
        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            None,
            &mut SuccessfulRunner,
            &mut inventory,
            &root.join("subc.jsonc"),
            &[],
        )
        .expect("uninstall");
        (root, ckni, report, inventory)
    }

    #[cfg(unix)]
    fn retained(report: &UninstallReport, path: &Path) -> bool {
        report.retained.contains(&format!(
            "modified managed destination retained: {}",
            path.display()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_keeps_a_regular_file_that_replaced_the_ckni_link() {
        let (_root, ckni, report, inventory) =
            uninstall_ckni("ckni-replaced-by-file", |ck, ckni| {
                fs::write(ck, "ck").unwrap();
                fs::write(ckni, "someone's own ckni").unwrap();
            });
        assert_eq!(fs::read_to_string(&ckni).unwrap(), "someone's own ckni");
        assert!(retained(&report, &ckni), "{report:?}");
        assert!(!report.removed.contains(&ckni));
        assert!(inventory.owns_path(super::super::ckni::CKNI_KIND, &ckni));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_keeps_a_ckni_link_that_points_somewhere_else() {
        let (root, ckni, report, _) = uninstall_ckni("ckni-points-elsewhere", |ck, ckni| {
            fs::write(ck, "ck").unwrap();
            let elsewhere = ck.with_file_name("elsewhere");
            fs::write(&elsewhere, "another program").unwrap();
            std::os::unix::fs::symlink(&elsewhere, ckni).unwrap();
        });
        assert_eq!(fs::read_link(&ckni).unwrap(), root.join("elsewhere"));
        assert!(retained(&report, &ckni), "{report:?}");
        assert!(!report.removed.contains(&ckni));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_removes_a_dangling_ckni_link_that_still_points_at_ck() {
        // ck is already gone, so the link dangles; it is still setup's link.
        let (_root, ckni, report, inventory) = uninstall_ckni("ckni-dangling", |_ck, ckni| {
            std::os::unix::fs::symlink("ck", ckni).unwrap();
        });
        assert!(
            fs::symlink_metadata(&ckni).is_err(),
            "the dangling link remains"
        );
        assert_eq!(report.removed, std::slice::from_ref(&ckni));
        assert!(!retained(&report, &ckni));
        assert!(!inventory.owns_path(super::super::ckni::CKNI_KIND, &ckni));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_removes_a_ckni_link_to_ck_and_leaves_ck_for_its_own_row() {
        let (root, ckni, report, _) = uninstall_ckni("ckni-live", |ck, ckni| {
            fs::write(ck, "ck").unwrap();
            std::os::unix::fs::symlink(ck, ckni).unwrap();
        });
        assert!(fs::symlink_metadata(&ckni).is_err());
        assert_eq!(report.removed, std::slice::from_ref(&ckni));
        // Nothing owns ck in this fixture, so it stays.
        assert!(root.join("ck").exists());
    }
}
