use std::path::{Path, PathBuf};

use super::{
    files::{self, Kind, Reading},
    journal::{
        durable,
        paths::{self, error_at},
        Action, RestoredTarget, Snapshot, Step,
    },
};

#[derive(Clone)]
pub(super) struct Undo {
    pub index: usize,
    pub target: PathBuf,
    pub kind: Kind,
    pub prior: Reading,
    expected: Reading,
    parent: subc_os::RetainedFileIdentity,
    placed: Option<String>,
}

fn placement(step: &Step) -> Option<(Kind, Reading, Option<&str>)> {
    let (prior, placed) = step.action.placement()?;
    let (kind, mode) = match step.action {
        Action::File { mode, .. } => (Kind::File, mode),
        Action::Bundle { .. } => (Kind::Bundle, None),
        _ => unreachable!("only file and bundle steps have placed content"),
    };
    Some((
        kind,
        Reading {
            digest: prior.map(str::to_owned),
            mode,
        },
        placed,
    ))
}

pub(super) fn saved(dir: &Path, index: usize) -> PathBuf {
    dir.join("saved").join(index.to_string())
}

fn aside(dir: &Path, index: usize) -> PathBuf {
    dir.join("aside").join(index.to_string())
}

pub(super) fn plan(dir: &Path, snapshot: &Snapshot) -> Result<Vec<Undo>, String> {
    let mut plan = Vec::<Undo>::new();
    for (i, entry) in snapshot
        .steps
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| !entry.undone)
    {
        let Some((kind, prior, placed)) = placement(&entry.step) else {
            continue;
        };
        let index = i + 1;
        let target = &entry.step.target;
        if let Some(sha) = &prior.digest {
            let source = saved(dir, index);
            files::check_parent(&source)?;
            kind.verify(&source, sha)?;
        }
        let newer = plan
            .iter()
            .rev()
            .find(|undo| paths::same_path(&undo.target, target));
        let expected = if let Some(newer) = newer {
            newer.prior.clone()
        } else {
            kind.snapshot(target)?
        };
        let live = expected.digest.as_deref();
        let previous = prior.digest.as_deref();
        if previous.is_none() || kind == Kind::Bundle && live != previous {
            files::check_absent_parent(target)?;
        }
        let resume = snapshot.revert_owner.is_some();
        let placed_or_prior =
            live == placed || (newer.is_none() && (!entry.done || resume) && live == previous);
        if !placed_or_prior {
            let aside = aside(dir, index);
            let was_moved = newer.is_none()
                && resume
                && kind == Kind::Bundle
                && live.is_none()
                && paths::info(&aside)?.is_some();
            if !was_moved {
                return Err(error_at(
                    target,
                    format!(
                        "step {index}: expected placed sha256 {}, found {}",
                        placed.unwrap_or("absent"),
                        live.unwrap_or("absent")
                    ),
                ));
            }
            files::check_parent(&aside)?;
            kind.verify(&aside, placed.expect("bundles are placed"))?;
        }
        plan.push(Undo {
            index,
            target: target.clone(),
            kind,
            prior,
            expected,
            parent: files::parent_identity(target)?,
            placed: placed.map(str::to_owned),
        });
    }
    Ok(plan)
}

impl Undo {
    pub(super) fn apply(&self, dir: &Path) -> Result<(), String> {
        let Self {
            index,
            target,
            kind,
            prior,
            expected,
            parent,
            placed,
        } = self;
        files::require_parent(target, parent)?;
        files::require_reading(*kind, target, expected)?;
        if *kind == Kind::Bundle {
            let aside = aside(dir, *index);
            if paths::info(&aside)?.is_some() {
                files::check_parent(&aside)?;
                kind.verify(&aside, placed.as_deref().expect("bundles are placed"))?;
                durable::parent(&aside)?;
            }
        }
        if expected != prior {
            let mut current = expected.clone();
            if let Some(sha) = &current.digest {
                if *kind == Kind::Bundle {
                    let dest = aside(dir, *index);
                    durable::ensure_dir(dir, dest.parent().expect("aside parent"))?;
                    files::require_reading(*kind, target, expected)?;
                    files::set_aside(*kind, target, &dest, sha)?;
                    current = Reading::default();
                }
            }
            if let Some(sha) = &prior.digest {
                let source = saved(dir, *index);
                files::check_parent(&source)?;
                files::restore_copy(*kind, &source, target, sha, prior.mode, &current)?;
            } else if current.digest.is_some() {
                files::remove_checked(*kind, target, &current)?;
            }
        }
        // A previous process may have published the prior bytes and failed its
        // directory sync. Establish durability here even when no write is needed.
        durable::parent(target)?;
        files::require_parent(target, parent)?;
        files::require_reading(*kind, target, prior)
    }
}

pub(super) fn final_targets(snapshot: &Snapshot) -> Vec<RestoredTarget> {
    let mut targets = Vec::<RestoredTarget>::new();
    for entry in &snapshot.steps {
        let Some((_, prior, _)) = placement(&entry.step) else {
            continue;
        };
        let target = &entry.step.target;
        if !targets
            .iter()
            .any(|row| paths::same_path(&row.target, target))
        {
            targets.push(RestoredTarget {
                target: target.clone(),
                sha256: prior.digest,
                mode: prior.mode,
            });
        }
    }
    targets
}

pub(super) fn check_restored(
    snapshot: &Snapshot,
    targets: &[RestoredTarget],
) -> Result<(), String> {
    for row in targets {
        let kind = snapshot
            .steps
            .iter()
            .find_map(|entry| {
                paths::same_path(&entry.step.target, &row.target)
                    .then(|| placement(&entry.step))
                    .flatten()
                    .map(|(kind, _, _)| kind)
            })
            .expect("restored target has a file or bundle step");
        files::require_reading(
            kind,
            &row.target,
            &Reading {
                digest: row.sha256.clone(),
                mode: row.mode,
            },
        )?;
    }
    Ok(())
}
