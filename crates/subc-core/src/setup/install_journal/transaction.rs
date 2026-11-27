//! A component's ownership is saved only when its journal is sealed. Each
//! setter takes the transaction explicitly so ordinary observations and caches
//! cannot start an install transaction as a side effect.

use super::{files, Journal, Outcome, Recovery};
use crate::setup::inventory::Inventory;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(in crate::setup) struct Transaction {
    manifest: PathBuf,
    journal: Journal,
}

impl Transaction {
    #[cfg(unix)]
    pub fn id(&self) -> &str {
        self.journal.id()
    }
    pub fn begin(manifest: &Path) -> Result<Self, String> {
        Ok(Self {
            manifest: manifest.to_owned(),
            journal: Journal::create(manifest)?,
        })
    }

    pub fn resume(manifest: &Path) -> Result<Self, String> {
        let journal = Journal::open(manifest)?
            .ok_or("refusal: install journal disappeared while waiting for confirmation")?;
        Ok(Self {
            manifest: manifest.to_owned(),
            journal,
        })
    }

    pub fn journal(&mut self) -> &mut Journal {
        &mut self.journal
    }

    pub fn write(&mut self, target: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
        files::place(
            &mut self.journal,
            target,
            files::Kind::File,
            files::Content::Bytes(bytes, mode),
        )
    }

    pub fn write_create_only(
        &mut self,
        target: &Path,
        bytes: &[u8],
        mode: u32,
    ) -> Result<bool, String> {
        files::create_only(&mut self.journal, target, bytes, mode)
    }

    pub fn replace(&mut self, target: &Path, candidate: &Path) -> Result<(), String> {
        files::place(
            &mut self.journal,
            target,
            files::Kind::File,
            files::Content::File(candidate),
        )
    }

    pub fn remove(&mut self, target: &Path) -> Result<(), String> {
        files::place(
            &mut self.journal,
            target,
            files::Kind::File,
            files::Content::Absent,
        )
    }

    pub fn commit(
        mut self,
        inventory: &Inventory,
        recovery: &mut impl Recovery,
    ) -> Result<Outcome, String> {
        if self.journal.next_index() == 0 && !inventory.has_changes() {
            return self.journal.recover(recovery);
        }
        let bytes = inventory.serialized();
        files::place(
            &mut self.journal,
            &self.manifest,
            files::Kind::Inventory,
            files::Content::Bytes(bytes.as_bytes(), 0o644),
        )?;
        self.journal.commit(recovery)
    }

    /// Prepare the ownership update without publishing it. Recovery can
    /// publish it after the daemon or module proves it is running the new
    /// release; otherwise recovery restores the previous installation.
    pub fn leave_unconfirmed(mut self, inventory: &Inventory) -> Result<(), String> {
        let data = inventory.serialized();
        files::prepare(
            &mut self.journal,
            &self.manifest,
            files::Kind::Inventory,
            &files::Content::Bytes(data.as_bytes(), 0o644),
        )?;
        Ok(())
    }

    pub fn undo(self, recovery: &mut impl Recovery) -> Result<Outcome, String> {
        if self.journal.poisoned {
            let id = self.journal.id().to_owned();
            drop(self.journal);
            let journal = Journal::open(&self.manifest)?.ok_or_else(|| {
                format!(
                    "refusal: {} disappeared after an append error",
                    Journal::path(&self.manifest).display()
                )
            })?;
            if journal.state.id.as_deref() != Some(id.as_str()) {
                return Err(format!(
                    "refusal: {} changed identity after an append error",
                    Journal::path(&self.manifest).display()
                ));
            }
            return journal.recover(recovery);
        }
        self.journal.recover(recovery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::install_journal::bootstrap::FileRecovery;
    use serde_json::{Map, Value};
    use std::fs;

    #[test]
    fn undo_reopens_a_poisoned_journal_and_restores_the_previous_bytes() {
        use crate::setup::install_journal::{Record, Undo};
        use std::io::Write;

        let root = cortexkit_test_support::ScratchDir::new("poisoned-undo");
        let manifest = root.join("manifest");
        let target = root.join("settings");
        fs::write(&target, "old settings").unwrap();
        let mut transaction = Transaction::begin(&manifest).unwrap();
        transaction.write(&target, b"new settings", 0o600).unwrap();
        transaction
            .journal
            .append_with(
                Record::Undo {
                    phase: Undo::Quiesce,
                },
                |file, bytes| {
                    file.write_all(&bytes[..3])?;
                    Err(std::io::Error::other("injected append I/O failure"))
                },
            )
            .unwrap_err();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new settings");
        assert_eq!(
            transaction.undo(&mut FileRecovery).unwrap(),
            Outcome::Undone
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "old settings");
        assert!(!Journal::exists(&manifest).unwrap());
    }

    #[test]
    fn a_component_error_does_not_undo_an_earlier_committed_component() {
        let scratch = cortexkit_test_support::ScratchDir::new("journal-component-isolation");
        let root = fs::canonicalize(scratch.path()).unwrap();
        let manifest = root.join("manifest");
        let target = root.join("component");
        let config = root.join("config");
        let candidate = root.join("candidate");
        fs::write(&candidate, b"installed image").unwrap();
        let mut ownership = Inventory::read(manifest.clone(), "test").unwrap();
        let mut first = Transaction::begin(&manifest).unwrap();
        first.replace(&target, &candidate).unwrap();
        first.write(&config, b"core settings", 0o600).unwrap();
        ownership.record(
            "managed-binary",
            &target,
            Map::from_iter([("component".into(), Value::String("core".into()))]),
        );
        assert!(!manifest.exists());
        assert_eq!(
            first.commit(&ownership, &mut FileRecovery).unwrap(),
            Outcome::Committed
        );
        let mut second = Transaction::begin(&manifest).unwrap();
        second.remove(&target).unwrap();
        second
            .write(&config, b"optional component settings", 0o600)
            .unwrap();
        assert_eq!(second.undo(&mut FileRecovery).unwrap(), Outcome::Undone);
        assert_eq!(fs::read(&config).unwrap(), b"core settings");
        assert_eq!(fs::read(&target).unwrap(), b"installed image");
        assert!(Inventory::read(manifest, "test")
            .unwrap()
            .owns_path("managed-binary", &target));
    }

    #[test]
    fn a_sealed_unconfirmed_transaction_leaves_the_manifest_unchanged_until_recovered() {
        let scratch = cortexkit_test_support::ScratchDir::new("journal-unconfirmed");
        let root = fs::canonicalize(scratch.path()).unwrap();
        let manifest = root.join("manifest");
        let target = root.join("binary");
        let mut ownership = Inventory::read(manifest.clone(), "test").unwrap();
        let mut transaction = Transaction::begin(&manifest).unwrap();
        assert_eq!(transaction.journal().next_index(), 0);
        transaction.write(&target, b"new", 0o700).unwrap();
        ownership.record("managed-binary", &target, Map::new());
        transaction.leave_unconfirmed(&ownership).unwrap();
        assert!(!manifest.exists());
        assert_eq!(
            Journal::open(&manifest)
                .unwrap()
                .unwrap()
                .recover(&mut FileRecovery)
                .unwrap(),
            Outcome::Committed
        );
        assert!(Inventory::read(manifest, "test")
            .unwrap()
            .owns_path("managed-binary", &target));
    }
}
