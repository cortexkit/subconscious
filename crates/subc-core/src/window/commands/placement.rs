use std::path::{Path, PathBuf};

use super::{
    super::{
        journal::{
            self, content, durable, paths, Action, Activation, Floor, Owner, Role, Snapshot,
            SnapshotTransfer, Step,
        },
        valid_module, WindowError,
    },
    host::Host,
};

pub const RECORD_HELP: &str = "Record the daemon placement by hand before replacing its file (or Subconscious.app bundle on macOS).\nA store snapshot is supported only if its format floors did not change between taking the snapshot and recording it. The window moves a snapshot on the same filesystem and copies it across filesystems.";

#[derive(Clone, Debug)]
pub enum RecordRequest {
    /// `new: None` records a removal.
    File {
        target: PathBuf,
        new: Option<PathBuf>,
        module: Option<String>,
        role: Option<Role>,
    },
    Bundle {
        target: PathBuf,
        new: PathBuf,
    },
    Store {
        store: PathBuf,
        snapshot: PathBuf,
        module: String,
    },
    /// The daemon or a supervised module ID.
    Activation {
        target: String,
    },
}

pub(super) fn prepare(
    root: &Path,
    journal: &Snapshot,
    request: RecordRequest,
) -> Result<RecordRequest, String> {
    // Resolve every path and refuse overlaps before reading content at any of them.
    match request {
        RecordRequest::File {
            target,
            new,
            module,
            role,
        } => {
            if role.is_some() && module.is_none()
                || module.as_ref().is_some_and(|id| !valid_module(id))
            {
                return Err("invalid module or file role (a role requires a module ID)".into());
            }
            let target = paths::placement(root, &target)?;
            journal.check_overlap(&target, "file")?;
            let new = new.map(|new| paths::placement(root, &new)).transpose()?;
            Ok(RecordRequest::File {
                target,
                new,
                module,
                role,
            })
        }
        RecordRequest::Bundle { target, new } => {
            let target = paths::placement(root, &target)?;
            journal.check_overlap(&target, "bundle")?;
            let new = paths::placement(root, &new)?;
            Ok(RecordRequest::Bundle { target, new })
        }
        RecordRequest::Store {
            store,
            snapshot,
            module,
        } => {
            if !valid_module(&module) {
                return Err(format!("invalid module ID {module:?}"));
            }
            let store = paths::placement(root, &store)?;
            let parent = store.parent().ok_or("store path has no parent")?;
            paths::in_domain(root, parent)?;
            let snapshot = paths::placement(root, &snapshot)?;
            if ["", "-wal", "-shm", "-journal"].into_iter().any(|suffix| {
                let mut live = store.as_os_str().to_owned();
                live.push(suffix);
                paths::same_path(&snapshot, Path::new(&live))
            }) {
                return Err(paths::error_at(
                    &snapshot,
                    "provide a separate snapshot, not the live store or a sidecar",
                ));
            }
            Ok(RecordRequest::Store {
                store,
                snapshot,
                module,
            })
        }
        RecordRequest::Activation { target } if target == "daemon" || valid_module(&target) => {
            Ok(RecordRequest::Activation { target })
        }
        RecordRequest::Activation { .. } => {
            Err("activation requires `daemon` or a valid module ID".into())
        }
    }
}

pub(super) fn materialize(
    request: RecordRequest,
    dir: &Path,
    journal: &Snapshot,
    index: usize,
    owner: Owner,
    host: &mut impl Host,
) -> Result<Step, WindowError> {
    let saved = dir.join("saved");
    if paths::plain_dir(&saved)? {
        let referenced: std::collections::BTreeSet<_> = journal::saved_indices(journal)
            .map(|index| index.to_string())
            .collect();
        for path in journal::entries(&saved)? {
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| referenced.contains(name))
            {
                durable::remove_artifact(&path)?;
            }
        }
    }
    let temporary = saved.join(format!(".tmp-{index}"));
    let copy = saved.join(index.to_string());
    let (target, action, module, role) = match request {
        RecordRequest::File {
            target,
            new,
            module,
            role,
        } => {
            let info = content::file_info(&target)?;
            if info.is_none() && new.is_none() {
                return Err(
                    paths::error_at(&target, "cannot record removal of an absent file").into(),
                );
            }
            let after = new
                .as_ref()
                .map(|path| required_digest(path, content::file_digest))
                .transpose()?;
            let before = if let Some(info) = &info {
                prepare_space(dir, &saved, info.len())?;
                let digest = content::copy_checked(&target, &temporary)?;
                durable::rename_new(&temporary, &copy)?;
                Some(digest)
            } else {
                None
            };
            let mode = info.as_ref().and_then(content::mode);
            (
                target,
                Action::File {
                    before,
                    after,
                    mode,
                },
                module,
                role,
            )
        }
        RecordRequest::Bundle { target, new } => {
            let after = required_digest(&new, content::bundle_digest)?;
            let before = if paths::plain_dir(&target)? {
                prepare_space(dir, &saved, content::tree_size(&target)?)?;
                let digest = content::bundle_copy_checked(&target, &temporary)?;
                durable::rename_new(&temporary, &copy)?;
                Some(digest)
            } else {
                None
            };
            (target, Action::Bundle { before, after }, None, None)
        }
        RecordRequest::Store {
            store,
            snapshot,
            module,
        } => {
            let tree = store.parent().expect("validated store path");
            content::file_info(&store)?;
            let snapshot_size = file_size(&snapshot)?;
            let floor_paths = content::floor_paths(tree)?;
            if floor_paths
                .iter()
                .any(|floor| paths::same_path(&snapshot, &tree.join(floor)))
            {
                return Err(paths::error_at(
                    &snapshot,
                    "provide a separate snapshot, not a live format floor",
                )
                .into());
            }
            let floor_size = floor_paths.iter().try_fold(0_u64, |sum, floor| {
                file_size(&tree.join(floor)).map(|bytes| sum.saturating_add(bytes))
            })?;
            if floor_size != 0 {
                prepare_space(dir, &saved, floor_size)?;
            }
            let floors_root = temporary.join("floors");
            durable::ensure_dir(dir, &floors_root)?;
            let mut floors = Vec::with_capacity(floor_paths.len());
            for path in floor_paths {
                let destination = floors_root.join(&path);
                durable::ensure_dir(
                    dir,
                    destination
                        .parent()
                        .expect("floor destination relative to snapshot"),
                )?;
                let sha256 = content::copy_checked(&tree.join(&path), &destination)?;
                floors.push(Floor { path, sha256 });
            }
            // Leave the external snapshot in place until all floor copies are safe.
            let destination = temporary.join("snapshot");
            let moved = content::move_snapshot(&snapshot, &destination).map_err(|error| {
                if error.renamed {
                    WindowError::failed(error.message)
                } else {
                    error.message.into()
                }
            })?;
            let (snapshot_sha256, snapshot_transfer) = match moved {
                Some(digest) => (digest, SnapshotTransfer::Moved),
                None => {
                    content::require_space(&saved, snapshot_size)?;
                    (
                        content::copy_checked(&snapshot, &destination)?,
                        SnapshotTransfer::Copied,
                    )
                }
            };
            durable::rename_new(&temporary, &copy).map_err(|error| {
                if snapshot_transfer == SnapshotTransfer::Moved {
                    WindowError::failed(error)
                } else {
                    error.into()
                }
            })?;
            (
                store,
                Action::Store {
                    snapshot_sha256,
                    snapshot_transfer,
                    floors,
                },
                Some(module),
                None,
            )
        }
        RecordRequest::Activation { target } => {
            let before = if target == "daemon" {
                let live = host.manager_live()?;
                let process = host.daemon()?;
                if live && process.is_none() {
                    return Err(
                        "the manager reports the daemon live, but its own identity is unavailable"
                            .into(),
                    );
                }
                daemon_placement(journal, &process, host)?;
                Activation::Daemon { process }
            } else {
                let module = host.module(&target)?;
                Activation::Module {
                    id: target.clone(),
                    spawn_generation: module.generation,
                    process: module.process,
                }
            };
            (
                PathBuf::from(target),
                Action::Activation { before },
                None,
                None,
            )
        }
    };
    Ok(Step {
        target,
        action,
        module,
        role,
        owner,
        config_generation: None,
    })
}

fn prepare_space(dir: &Path, saved: &Path, size: u64) -> Result<(), String> {
    // `saved` may not exist on the first copy; its parent is on the same filesystem.
    content::require_space(if paths::plain_dir(saved)? { saved } else { dir }, size)?;
    durable::ensure_dir(dir, saved)
}

fn file_size(path: &Path) -> Result<u64, String> {
    content::file_info(path)?
        .map(|info| info.len())
        .ok_or_else(|| paths::error_at(path, "file is absent"))
}

fn required_digest(
    path: &Path,
    digest: impl Fn(&Path) -> Result<Option<String>, String>,
) -> Result<String, String> {
    digest(path)?.ok_or_else(|| paths::error_at(path, "source is absent"))
}

fn daemon_placement(
    journal: &Snapshot,
    process: &Option<Owner>,
    host: &impl Host,
) -> Result<(), String> {
    let path = host.daemon_path()?;
    let executable = paths::canonical(&path)?;
    let bundle = path
        .ancestors()
        .find(|ancestor| ancestor.extension().is_some_and(|ext| ext == "app"))
        .map(paths::canonical)
        .transpose()?;
    let placement = journal.steps.iter().rev().find(|entry| {
        entry.done
            && match entry.step.action {
                Action::File { .. } => paths::same_path(&entry.step.target, &executable),
                Action::Bundle { .. } => bundle
                    .as_ref()
                    .is_some_and(|path| paths::same_path(&entry.step.target, path)),
                _ => false,
            }
    });
    let (matches, subject) = if let Some(entry) = placement {
        let subject = &entry.step.target;
        let actual = if matches!(entry.step.action, Action::Bundle { .. }) {
            content::bundle_digest(subject)?
        } else {
            content::file_digest(subject)?
        };
        let placed = entry
            .step
            .action
            .placement()
            .expect("file or bundle step")
            .1;
        (placed.is_some() && placed == actual.as_deref(), subject)
    } else {
        (
            host.has_running_image_proof()
                && process
                    .as_ref()
                    .is_some_and(|owner| host.image_matches(owner, &executable)),
            &path,
        )
    };
    if matches {
        return Ok(());
    }
    Err(paths::error_at(
        subject,
        "a daemon change not recorded in this window cannot be undone by it",
    ))
}

pub(super) fn verify_done(root: &Path, step: &Step, host: &mut impl Host) -> Result<(), String> {
    if step.action.placement().is_some() {
        let resolved = paths::placement(root, &step.target)?;
        if !paths::same_path(&resolved, &step.target) {
            return Err(paths::error_at(
                &step.target,
                "placement target resolves to a different path",
            ));
        }
    }
    match &step.action {
        Action::File { after, .. } => {
            check_digest(&step.target, after.as_deref(), content::file_digest)
        }
        Action::Bundle { after, .. } => {
            check_digest(&step.target, Some(after), content::bundle_digest)
        }
        Action::Store { .. } => Ok(()),
        Action::Activation {
            before: Activation::Daemon { process },
        } => {
            if !host.manager_live()? {
                return Err("the manager does not report the daemon live".into());
            }
            let now = host
                .daemon()?
                .ok_or("the daemon did not report its live incarnation")?;
            if process
                .as_ref()
                .is_some_and(|before| before.same_process(&now))
            {
                return Err(format!(
                    "the daemon is still the recorded incarnation ({})",
                    now.describe()
                ));
            }
            if host.has_running_image_proof() {
                let path = host.daemon_path()?;
                if !host.image_matches(&now, &path) {
                    return Err(paths::error_at(
                        &path,
                        "the daemon running image does not match the file in place",
                    ));
                }
            }
            Ok(())
        }
        Action::Activation {
            before:
                Activation::Module {
                    id,
                    spawn_generation,
                    ..
                },
        } => {
            let now = host.module(id)?;
            if now.process.is_none() {
                return Err(format!("module {id} has no proven live process"));
            }
            if now.generation == *spawn_generation {
                return Err(format!(
                    "module {id} is still on spawn generation {spawn_generation}"
                ));
            }
            if now.image_disagrees {
                return Err(format!("module {id} reports a running-image disagreement"));
            }
            Ok(())
        }
    }
}

fn check_digest(
    path: &Path,
    expected: Option<&str>,
    digest: impl Fn(&Path) -> Result<Option<String>, String>,
) -> Result<(), String> {
    let actual = digest(path)?;
    if expected != actual.as_deref() {
        return Err(paths::error_at(
            path,
            format!(
                "expected digest {}, found {}",
                expected.unwrap_or("absent"),
                actual.as_deref().unwrap_or("absent")
            ),
        ));
    }
    Ok(())
}
