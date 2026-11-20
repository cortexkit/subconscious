use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    components::digest_file,
    inventory::Inventory,
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform},
};

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
    let mut report = UninstallReport::default();
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

    report
        .retained
        .push(format!("configuration: {}", config_path.display()));
    for store in store_paths {
        report.retained.push(format!("store: {}", store.display()));
    }
    Ok(report)
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
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn fixture_dir(name: &str) -> ScratchDir {
        ScratchDir::new(name)
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
