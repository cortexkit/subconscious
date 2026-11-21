use super::{files, Action, Journal, Outcome, Recovery, Step, Undo};
use crate::setup::{apply, install_lock::InstallHold, inventory::Inventory, PlatformObservation};
use serde_json::{Map, Value};
use std::ffi::OsString;
#[cfg(any(unix, test))]
use std::fs;
use std::path::{Path, PathBuf};

const VERB: &str = "__install-bootstrap";

/// This entry point runs only in the archive's private extraction directory.
/// The bootstrap scripts check the release archive before invoking it.
pub fn entry(argv: &[OsString]) -> Option<Result<(), String>> {
    if argv.get(1)? != VERB {
        return None;
    }
    Some((|| {
        let [_, _, digest, version] = argv else {
            return Err(format!(
                "refusal: {VERB} requires the archive digest and release version"
            ));
        };
        let digest = digest
            .to_str()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or("refusal: bootstrap archive digest is not SHA-256")?;
        let version = version
            .to_str()
            .filter(|value| semver::Version::parse(value).is_ok())
            .ok_or("refusal: bootstrap release version is not a semantic version")?;
        let source = std::env::current_exe()
            .map_err(|error| format!("could not locate the bootstrap ck: {error}"))?;
        verify_extraction(&source)?;
        let root = apply::data_directory()?;
        place(
            &root,
            &source,
            digest.to_ascii_lowercase(),
            version.to_owned(),
        )
    })())
}

fn verify_extraction(source: &Path) -> Result<(), String> {
    let invalid = |detail: &str| {
        format!("refusal: bootstrap ck {} must run from a private temporary extraction directory ({detail})", source.display())
    };
    let temp = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| invalid("temporary directory is unavailable"))?;
    let canonical = source
        .canonicalize()
        .map_err(|_| invalid("could not resolve the executable"))?;
    let mut directory = canonical
        .parent()
        .ok_or_else(|| invalid("executable has no directory"))?;
    // The extracted executable may be in a subdirectory of the archive's
    // private temporary directory. Do not accept the shared temp root itself.
    while directory.starts_with(&temp) && directory != temp {
        if private_directory(directory) {
            return Ok(());
        }
        directory = directory
            .parent()
            .ok_or_else(|| invalid("extraction path has no parent"))?;
    }
    Err(invalid("no owner-only directory below the temporary root"))
}

#[cfg(unix)]
fn private_directory(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|info| {
        info.is_dir()
            && info.uid() == rustix::process::geteuid().as_raw()
            && info.mode() & 0o077 == 0
    })
}

#[cfg(windows)]
fn private_directory(path: &Path) -> bool {
    subc_os::windows_acl::verify_private_dir(path).is_ok()
}

pub(super) fn destination(root: &Path) -> PathBuf {
    root.join("cmd")
        .join(if cfg!(windows) { "ck.exe" } else { "ck" })
}

pub(super) fn place(
    root: &Path,
    source: &Path,
    archive_sha256: String,
    version: String,
) -> Result<(), String> {
    let manifest = root.join("installer-manifest.json");
    let _lock = InstallHold::take(
        &manifest,
        "ck bootstrap placement",
        crate::setup::install_lock::WAIT,
    )?;
    super::retired::sweep(&manifest)?;
    let target = destination(root);
    if let Some(journal) = Journal::open(&manifest)? {
        if let Some(step) = journal.handover() {
            if step.target != target || !handover_was_interrupted(&journal, step)? {
                return Err(waiting_for_setup(&manifest));
            }
        } else if journal.state.outcome.is_none()
            && journal.state.id.is_some()
            && files::digest(&target)?.is_some()
        {
            let mut legacy_gap = false;
            for step in &journal.state.steps {
                if step.action.file().is_some()
                    && super::legacy::same_sibling(&step.target, &root.join("bin/ck.exe"))
                    && files::is_replacement_gap(journal.id(), step)?
                {
                    legacy_gap = true;
                    break;
                }
            }
            if !legacy_gap {
                return Err(waiting_for_setup(&manifest));
            }
        }
        drop(journal);
        let platform = crate::setup::runtime::RuntimePlatform::current();
        let paths = crate::setup::runtime::session_runtime_paths(
            platform,
            &root.join("bin"),
            &apply::user_home()?,
        );
        let mut runner = crate::setup::runtime::SystemCommandRunner;
        let mut recovery = super::system::SystemRecovery {
            executable: source,
            connection: None,
            platform,
            paths: &paths,
            runner: &mut runner,
        };
        // If the installed command is missing, the verified helper can use
        // exactly the same recovery checks even if the journal names other files.
        recover_locked(&manifest, &mut recovery, true).map_err(|error| {
            format!(
                "could not recover {}: {error}",
                Journal::path(&manifest).display()
            )
        })?;
        if Journal::exists(&manifest)? {
            return Err(format!(
                "refusal: {} was not closed by recovery",
                Journal::path(&manifest).display()
            ));
        }
    }

    super::legacy::reconcile(&manifest)?;
    let inventory = read_inventory(&manifest)?;
    refuse_legacy(&manifest, &inventory)?;
    if let Some(actual) = files::digest(&target)? {
        let tracked = super::super::components::OWNED_BINARY_KINDS
            .iter()
            .any(|kind| {
                inventory
                    .paths_for_kind(kind)
                    .into_iter()
                    .filter(|path| super::legacy::same_sibling(path, &target))
                    .any(|path| {
                        inventory
                            .entry_for_path(kind, &path)
                            .and_then(|row| row.get("sha256"))
                            .and_then(Value::as_str)
                            == Some(&actual)
                    })
            });
        if !tracked && files::digest(source)?.as_deref() != Some(&actual) {
            return Err(format!(
                "refusal: {} is not the recorded ck executable",
                target.display()
            ));
        }
    }
    let mut journal = Journal::create(&manifest)?;
    files::bootstrap(&mut journal, &target, source, archive_sha256, version)?;
    println!(
        "Placed ck at {}; ck setup will record ownership.",
        target.display()
    );
    Ok(())
}

fn waiting_for_setup(manifest: &Path) -> String {
    format!("refusal: an install transaction is waiting at {}; run ck setup using the installed ck, then retry the installer", Journal::path(manifest).display())
}

fn handover_was_interrupted(journal: &Journal, step: &Step) -> Result<bool, String> {
    if journal.state.done.first() == Some(&true) && files::verify(step)? {
        return Ok(false);
    }
    let file = step.action.file().expect("handover has a file step");
    Ok(files::digest(&step.target)? == file.before
        || files::is_replacement_gap(journal.id(), step)?)
}

pub(in crate::setup) fn recover_locked(
    manifest: &Path,
    recovery: &mut impl Recovery,
    extracted_helper: bool,
) -> Result<(), String> {
    let Some(journal) = Journal::open(manifest)? else {
        return Ok(());
    };
    recover_open(manifest, journal, recovery, extracted_helper)
}

pub(super) fn recover_open(
    manifest: &Path,
    mut journal: Journal,
    recovery: &mut impl Recovery,
    extracted_helper: bool,
) -> Result<(), String> {
    if let Some(step) = journal.handover().cloned() {
        let root = manifest
            .parent()
            .ok_or("installer manifest has no parent")?;
        if step.target != destination(root) {
            return Err(format!(
                "refusal: bootstrap handover in {} names an unexpected executable {}",
                Journal::path(manifest).display(),
                step.target.display()
            ));
        }
        let retry = extracted_helper && handover_was_interrupted(&journal, &step)?;
        if retry || !files::verify(&step)? {
            journal.recover(recovery)?;
            if retry {
                return Ok(());
            }
            return Err(format!(
                "refusal: recovered an interrupted bootstrap of {}; run the installer again",
                step.target.display()
            ));
        }
        let Action::Bootstrap {
            file,
            archive_sha256,
            version,
        } = step.action
        else {
            unreachable!()
        };
        let binary_sha256 = file
            .after
            .ok_or("refusal: missing placed bootstrap digest")?;
        let mut inventory = read_inventory(manifest)?;
        refuse_legacy(manifest, &inventory)?;
        let mut fields = Map::new();
        fields.insert("component".into(), Value::String("core".into()));
        fields.insert("sha256".into(), Value::String(binary_sha256));
        fields.insert("archive_sha256".into(), Value::String(archive_sha256));
        fields.insert("version".into(), Value::String(version));
        for kind in crate::setup::components::OWNED_BINARY_KINDS {
            for old in inventory.paths_for_kind(kind) {
                if super::legacy::same_sibling(&old, &step.target) {
                    inventory.remove_owned_path(kind, &old);
                }
            }
        }
        inventory.record("managed-binary", &step.target, fields);
        files::place(
            &mut journal,
            manifest,
            files::Kind::Inventory,
            files::Content::Bytes(inventory.serialized().as_bytes(), 0o644),
        )?;
        journal.commit(recovery)?;
        crate::setup::messages::notice(format!(
            "Recorded bootstrap executable {}.",
            step.target.display()
        ));
        return Ok(());
    }
    let outcome = journal.recover(recovery)?;
    crate::setup::messages::notice(format!(
        "Recovered install transaction at {}: {}.",
        Journal::path(manifest).display(),
        if outcome == Outcome::Committed {
            "committed"
        } else {
            "rolled back"
        }
    ));
    Ok(())
}

pub(super) fn read_inventory(manifest: &Path) -> Result<Inventory, String> {
    Inventory::read(
        manifest.to_owned(),
        &PlatformObservation::current().to_string(),
    )
}

pub(in crate::setup) fn refuse_legacy(
    manifest: &Path,
    inventory: &Inventory,
) -> Result<(), String> {
    let mut unfinished = Vec::new();
    for kind in ["pending-activation", "daemon-bundle-sibling"] {
        for path in inventory.paths_for_kind(kind) {
            unfinished.push(format!(
                "{} ({kind} in {})",
                path.display(),
                manifest.display()
            ));
        }
    }
    for path in inventory.paths_for_kind("runtime-definition") {
        if let Some(old) = inventory
            .entry_for_path("runtime-definition", &path)
            .and_then(|row| row.get("moving_from"))
            .and_then(Value::as_str)
        {
            unfinished.push(format!(
                "{} and {old} (unit move in {})",
                path.display(),
                manifest.display()
            ));
        }
    }
    if unfinished.is_empty() {
        return Ok(());
    }
    Err(format!("refusal: an older installer left unfinished state: {}. Run ck setup with the ck already installed, then run the new installer again.", unfinished.join(", ")))
}

pub(in crate::setup) struct FileRecovery;

impl Recovery for FileRecovery {
    fn is_applied(&mut self, step: &Step) -> Result<bool, String> {
        #[cfg(windows)]
        if matches!(step.action, Action::UserPath { .. }) {
            return super::user_path::verify(step);
        }
        check_file(step)?;
        files::verify(step)
    }

    fn can_replay(&mut self, id: &str, step: &Step) -> Result<bool, String> {
        #[cfg(windows)]
        if matches!(step.action, Action::UserPath { .. }) {
            return super::user_path::can_replay(step);
        }
        check_file(step)?;
        files::can_replay(id, step)
    }

    fn replay(&mut self, id: &str, step: &Step) -> Result<(), String> {
        #[cfg(windows)]
        if matches!(step.action, Action::UserPath { .. }) {
            return super::user_path::replay(step);
        }
        check_file(step)?;
        files::replay(id, step)
    }

    fn undo(&mut self, id: &str, phase: Undo, step: &Step) -> Result<(), String> {
        #[cfg(windows)]
        if matches!(step.action, Action::UserPath { .. }) {
            return super::user_path::undo(step, phase);
        }
        check_file(step)?;
        if phase == Undo::Restore {
            files::restore(id, step)?;
        }
        Ok(())
    }
}

fn check_file(step: &Step) -> Result<(), String> {
    if matches!(
        step.action,
        Action::File(_) | Action::Inventory { .. } | Action::Bootstrap { .. }
    ) {
        Ok(())
    } else {
        Err(format!(
            "refusal: cannot recover an install service step at {}",
            step.target.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_updates_an_already_managed_executable_only_at_commit() {
        let scratch = cortexkit_test_support::ScratchDir::new("journal-bootstrap");
        let root = fs::canonicalize(scratch.path()).unwrap();
        let source = root.join("downloaded");
        fs::write(&source, b"tested update").unwrap();
        let target = destination(&root);
        fs::create_dir(target.parent().unwrap()).unwrap();
        fs::write(&target, b"previous executable").unwrap();
        let manifest = root.join("installer-manifest.json");
        let mut inventory = read_inventory(&manifest).unwrap();
        let old = files::digest(&target).unwrap().unwrap();
        inventory.record(
            "managed-binary",
            &target,
            Map::from_iter([("sha256".into(), Value::String(old.clone()))]),
        );
        inventory.save().unwrap();
        place(&root, &source, "c".repeat(64), "1.2.3".into()).unwrap();
        let on_disk = fs::read_to_string(&manifest).unwrap();
        assert!(on_disk.contains(&old));
        assert_eq!(fs::read(&target).unwrap(), b"tested update");
        let _lock = InstallHold::take(&manifest, "test handover", crate::setup::install_lock::WAIT)
            .unwrap();
        recover_locked(&manifest, &mut FileRecovery, false).unwrap();
        let inventory = read_inventory(&manifest).unwrap();
        let row = inventory.entry_for_path("managed-binary", &target).unwrap();
        assert_eq!(row["sha256"], files::digest(&source).unwrap().unwrap());
        assert_eq!(row["archive_sha256"], "c".repeat(64));
        assert_eq!(row["version"], "1.2.3");
        assert!(Journal::status(&manifest).unwrap().is_none());
    }

    #[test]
    fn handover_rewrites_raw_legacy_and_mixed_ownership_rows_to_one_current_row() {
        for mixed in [false, true] {
            let fixture = cortexkit_test_support::ScratchDir::new("journal-handover-ownership");
            let root = fs::canonicalize(fixture.path()).unwrap();
            let manifest = root.join("installer-manifest.json");
            let source = root.join("candidate");
            let target = destination(&root);
            fs::create_dir(target.parent().unwrap()).unwrap();
            fs::write(&target, b"old image").unwrap();
            fs::write(&source, b"new image").unwrap();
            let digest = files::digest(&target).unwrap().unwrap();
            let mut rows = vec![
                serde_json::json!({"kind":"binary-placement", "path":target, "sha256":digest, "version":"old-version"}),
            ];
            if mixed {
                rows.push(serde_json::json!({"kind":"managed-binary", "path":target, "sha256":"stale digest", "version":"stale version"}));
            }
            fs::write(
                &manifest,
                serde_json::to_vec(&serde_json::json!({"schema_version":1,"mutations":rows}))
                    .unwrap(),
            )
            .unwrap();
            place(&root, &source, "a".repeat(64), "8.7.6".into()).unwrap();
            let _hold =
                InstallHold::take(&manifest, "test handover", crate::setup::install_lock::WAIT)
                    .unwrap();
            recover_locked(&manifest, &mut FileRecovery, false).unwrap();
            let committed: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
            let rows = committed["mutations"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["kind"], "managed-binary");
            assert_eq!(rows[0]["version"], "8.7.6");
            assert_eq!(rows[0]["archive_sha256"], "a".repeat(64));
            assert_eq!(rows[0]["sha256"], files::digest(&source).unwrap().unwrap());
        }
    }

    #[test]
    fn a_legacy_pending_activation_is_named_and_is_never_imported() {
        let scratch = cortexkit_test_support::ScratchDir::new("journal-legacy-pending");
        let manifest = scratch.path().join("installer-manifest.json");
        let target = scratch.path().join("ck-subc");
        let mut inventory = read_inventory(&manifest).unwrap();
        inventory.record("pending-activation", &target, Map::new());
        let error = refuse_legacy(&manifest, &inventory).unwrap_err();
        assert!(error.contains(&target.display().to_string()), "{error}");
        assert!(error.contains(&manifest.display().to_string()), "{error}");
        assert!(
            error.contains("Run ck setup with the ck already installed"),
            "{error}"
        );
    }
}
