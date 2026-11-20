use super::install_journal::{files, Journal};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct PromptSnapshot(BTreeMap<PathBuf, Option<String>>);

impl PromptSnapshot {
    pub(super) fn capture(
        manifest: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, String> {
        [manifest.to_owned(), Journal::path(manifest)]
            .into_iter()
            .chain(paths)
            .map(|path| stamp(&path).map(|digest| (path, digest)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Self)
    }

    pub(super) fn check(&self) -> Result<(), String> {
        let mut changed = Vec::new();
        for (path, expected) in &self.0 {
            if stamp(path)? != *expected {
                changed.push(path.display().to_string());
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        Err(format!("refusal: {} changed while waiting for confirmation; rerun the installer command to review the new plan", changed.join(", ")))
    }
}

fn stamp(path: &Path) -> Result<Option<String>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => std::fs::read_link(path)
            .map(|target| Some(format!("symlink:{target:?}")))
            .map_err(|error| format!("could not inspect {}: {error}", path.display())),
        _ => files::digest(path),
    }
}
