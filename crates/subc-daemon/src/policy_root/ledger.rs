use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use subc_config::{raw_digest, Digest};

use super::{canonical_hex, Change};
use crate::config_gate::GateError;

const MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const CHECKPOINT_FIELD: &str = "policy_root_ledger";

pub(crate) fn last_checkpoint(
    body: &crate::config_store::StoreBody,
) -> Option<(u64, &serde_json::Value)> {
    body.generations.iter().rev().find_map(|(&id, entry)| {
        entry
            .generation
            .changeset
            .apply_plan
            .get(CHECKPOINT_FIELD)
            .map(|value| (id, value))
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    pub(crate) digest: Digest,
    pub(crate) row_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Departure {
    Rotated,
    Retired,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key_id: String,
    departure: Departure,
    unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_public_key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    statement_digest: Option<Digest>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct Ledger(Vec<Entry>);

impl Ledger {
    pub(crate) fn checkpoint(&self) -> Result<Checkpoint, GateError> {
        Ok(Checkpoint {
            digest: rows_digest(&self.0)?,
            row_count: self.0.len(),
        })
    }

    pub(crate) fn extends(&self, checkpoint: &Checkpoint) -> Result<(), GateError> {
        let prefix = self.0.get(..checkpoint.row_count).ok_or_else(problem)?;
        if rows_digest(prefix)? != checkpoint.digest {
            return Err(problem());
        }
        Ok(())
    }

    pub(crate) fn contains(&self, key_id: &str) -> bool {
        self.0.iter().any(|entry| entry.key_id == key_id)
    }

    pub(crate) fn retired(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|entry| entry.departure == Departure::Retired)
            .map(|entry| entry.key_id.clone())
            .collect()
    }

    pub(crate) fn interrupted_destination(&self, current: &str) -> Option<&str> {
        self.0
            .last()
            .filter(|entry| entry.key_id == current)
            .and_then(|entry| entry.to_public_key_hex.as_deref())
    }

    pub(crate) fn matches_interrupted(&self, change: &Change) -> bool {
        self.0.last().is_some_and(|entry| {
            change.previous.as_ref() == Some(&entry.key_id)
                && change.departure == Some(entry.departure)
                && entry.to_key_id.as_ref() == Some(&change.next.key_id)
                && entry.to_public_key_hex.as_ref() == Some(&change.next.public_key_hex)
                && entry.statement_digest.is_some()
                && change.statement_digest == entry.statement_digest
        })
    }

    pub(crate) fn read(path: &Path) -> Result<Option<Self>, GateError> {
        // An absent ledger is distinct from a ledger that could not be read or
        // verified. Neither can authorize a key already present in subc.jsonc.
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(problem()),
        }
        let file = subc_os::open_regular_nofollow_all(path).map_err(|_| problem())?;
        verify_file(&file).map_err(|_| problem())?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| problem())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(problem());
        }
        let ledger: Self = serde_json::from_slice(&bytes).map_err(|_| problem())?;
        let mut ids = std::collections::BTreeSet::new();
        if ledger.0.iter().any(|entry| {
            let complete = match (
                &entry.to_key_id,
                &entry.to_public_key_hex,
                &entry.statement_digest,
            ) {
                (Some(id), Some(hex), Some(_)) => {
                    *id != entry.key_id
                        && subc_protocol::policy_root::decode_public_key(hex)
                            .is_some_and(|raw| subc_protocol::policy_root::key_id(&raw) == *id)
                }
                (None, None, None) => entry.departure == Departure::Retired,
                _ => false,
            };
            !canonical_hex(&entry.key_id, 16) || !ids.insert(&entry.key_id) || !complete
        }) {
            return Err(problem());
        }
        Ok(Some(ledger))
    }

    pub(crate) fn read_checked(
        path: &Path,
        checkpoint: Option<&Checkpoint>,
    ) -> Result<Option<Self>, GateError> {
        let ledger = Self::read(path)?;
        if let Some(checkpoint) = checkpoint {
            ledger.as_ref().ok_or_else(problem)?.extends(checkpoint)?;
        }
        Ok(ledger)
    }

    pub(crate) fn apply(
        path: &Path,
        change: &Change,
        now_ms: u64,
        checkpoint: Option<&Checkpoint>,
        require_existing: bool,
    ) -> Result<Checkpoint, GateError> {
        let ledger = Self::read_checked(path, checkpoint)?;
        let exists = ledger.is_some();
        let mut ledger = ledger.unwrap_or_default();
        if ledger.contains(&change.next.key_id) {
            return Err(super::refuse("policy_root_key_reused"));
        }
        if ledger.matches_interrupted(change) {
            return ledger.checkpoint();
        }
        match (&change.previous, change.departure, &change.statement_digest) {
            (Some(key_id), Some(departure), Some(statement_digest))
                if exists && !ledger.contains(key_id) =>
            {
                ledger.0.push(Entry {
                    key_id: key_id.clone(),
                    departure,
                    unix_secs: now_ms / 1000,
                    to_key_id: Some(change.next.key_id.clone()),
                    to_public_key_hex: Some(change.next.public_key_hex.clone()),
                    statement_digest: Some(statement_digest.clone()),
                });
            }
            (None, None, None) if exists => return ledger.checkpoint(),
            (None, None, None) if !require_existing && !directory_exists(path)? => {}
            _ => return Err(problem()),
        }
        ledger.write(path)?;
        ledger.checkpoint()
    }

    pub(crate) fn retire_for_stamp(
        &mut self,
        path: &Path,
        key_id: String,
        now_ms: u64,
    ) -> Result<(), GateError> {
        if self.contains(&key_id) {
            return Ok(());
        }
        self.0.push(Entry {
            key_id,
            departure: Departure::Retired,
            unix_secs: now_ms / 1000,
            to_key_id: None,
            to_public_key_hex: None,
            statement_digest: None,
        });
        self.write(path)
    }

    fn write(&self, path: &Path) -> Result<(), GateError> {
        let bytes = serde_json::to_vec(self).map_err(|_| problem())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(problem());
        }
        durable_write(path, &bytes).map_err(|_| problem())
    }
}

fn rows_digest(rows: &[Entry]) -> Result<Digest, GateError> {
    // Digest a deterministic serialization of the prefix, not the spacing of
    // the ledger file. Later rows must not change the older prefix's digest.
    let bytes = serde_json::to_vec(rows).map_err(|_| problem())?;
    Ok(raw_digest(&bytes))
}

pub(crate) fn ledger_path(data: PathBuf) -> Option<PathBuf> {
    data.is_absolute()
        .then(|| data.join("policy-root").join("ledger.json"))
}

pub(crate) fn directory_exists(path: &Path) -> Result<bool, GateError> {
    match fs::symlink_metadata(path.parent().ok_or_else(problem)?) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(problem()),
    }
}

pub(crate) fn problem() -> GateError {
    GateError::new(
        "policy_root_ledger_unavailable",
        "agent_policy_root: policy-root/ledger.json is absent, invalid or unavailable; restore it from backup",
    )
}

fn verify_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        verify_owner_only_for_uid(&file.metadata()?, subc_os::effective_uid())
    }
    #[cfg(windows)]
    {
        subc_os::windows_acl::verify_owner_only(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(unix)]
fn verify_owner_only_for_uid(metadata: &fs::Metadata, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[test]
fn policy_root_review_refuses_wrong_ledger_owner() {
    use std::os::unix::fs::MetadataExt;

    let scratch = cortexkit_test_support::ScratchDir::new("ledger-owner-check");
    let path = scratch.join("ledger.json");
    let file = private_file(&path).unwrap();
    let metadata = file.metadata().unwrap();
    let owner = metadata.uid();
    verify_owner_only_for_uid(&metadata, owner).unwrap();
    assert_eq!(
        verify_owner_only_for_uid(&metadata, owner ^ 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

fn durable_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private_dir(dir)?;
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|_| io::Error::other("cannot allocate temporary file"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let temp = dir.join(format!(".ledger-{suffix}.tmp"));
    let mut file = private_file(&temp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result =
        written.and_then(|()| subc_os::durable_replace(&temp, path).map_err(io::Error::other));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != subc_os::effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
use subc_os::windows_acl::{
    create_private_dir as private_dir, create_private_file as private_file,
};

#[cfg(not(any(unix, windows)))]
fn private_dir(_: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(not(any(unix, windows)))]
fn private_file(_: &Path) -> io::Result<File> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}
