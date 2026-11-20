use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};

use super::{components::OWNED_BINARY_KINDS, inventory::Inventory};

/// Keep the legacy image in place through activation. Only a verified
/// replacement may retire it; rollback restores the inventory records that
/// identify which installed binaries setup is allowed to replace or delete.
pub(super) struct RetainedBinary {
    pub(super) old: PathBuf,
    new: PathBuf,
    rows: Vec<(String, Map<String, Value>)>,
}

impl RetainedBinary {
    pub(super) fn capture(old: &Path, new: &Path, inventory: &Inventory) -> Self {
        let rows = OWNED_BINARY_KINDS
            .iter()
            .filter_map(|kind| {
                inventory
                    .entry_for_path(kind, old)
                    .and_then(Value::as_object)
                    .map(|row| ((*kind).into(), row.clone()))
            })
            .collect();
        Self {
            old: old.into(),
            new: new.into(),
            rows,
        }
    }

    pub(super) fn complete(&self, inventory: &mut Inventory) -> Result<(), String> {
        if self.old != self.new {
            if !self.rows.is_empty() && self.old.is_file() {
                if let Some(transaction) = inventory.transaction() {
                    transaction.remove(&self.old)?;
                } else {
                    fs::remove_file(&self.old).map_err(|e| {
                        format!("could not remove legacy binary {}: {e}", self.old.display())
                    })?;
                }
            }
            for kind in OWNED_BINARY_KINDS {
                inventory.remove_owned_path(kind, &self.old);
            }
        }
        Ok(())
    }
}
