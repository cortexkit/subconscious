//! Exclusive, durable leases for release-train mutation.
//!
//! Lease acquisition uses the operating system's exclusive file lock and writes
//! a synchronized holder record only after the lock is held. A filesystem that
//! rejects either operation is refused; this module never substitutes a
//! best-effort lock or an unsynchronized marker file.

use crate::{RepositoryId, TrainId};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// The scope protected by one exclusive lease file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaseScope {
    RepositoryTrain {
        repository: RepositoryId,
        train: TrainId,
    },
    Repository {
        repository: RepositoryId,
    },
}

/// Diagnostic information written by the holder after it acquires a lock.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseHolder {
    pub process_id: u32,
    pub acquired_at_ms: u128,
}

/// Fail-closed lease errors suitable for typed execution refusals.
#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("lease conflict for {scope:?}; live holder: {holder:?}")]
    Conflict {
        scope: LeaseScope,
        holder: Option<LeaseHolder>,
    },
    #[error("lease identity contains an unsafe path component: {0}")]
    UnsafeIdentity(String),
    #[error("filesystem at {path} does not support required exclusive locking: {source}")]
    UnsupportedLocking {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("filesystem at {path} does not support required durability: {source}")]
    UnsupportedDurability {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("unable to access lease state at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("lease holder record at {path} is corrupt: {reason}")]
    CorruptHolder { path: PathBuf, reason: String },
}

/// A durable-state root that can acquire release-machine leases.
#[derive(Clone, Debug)]
pub struct LeaseStore {
    state_root: PathBuf,
}

impl LeaseStore {
    /// Opens a state root only when its directory can be synchronized durably.
    pub fn new(state_root: impl Into<PathBuf>) -> Result<Self, LeaseError> {
        let state_root = state_root.into();
        fs::create_dir_all(&state_root).map_err(|source| LeaseError::Io {
            path: state_root.clone(),
            source,
        })?;
        sync_directory(&state_root)?;
        Ok(Self { state_root })
    }

    /// Acquires the exclusive lease shared only by this repository and train.
    pub fn acquire_train(
        &self,
        repository: RepositoryId,
        train: TrainId,
    ) -> Result<LeaseGuard, LeaseError> {
        self.acquire(LeaseScope::RepositoryTrain { repository, train })
    }

    /// Acquires the exclusive lease shared by all tree-mutating trains in a repository.
    pub fn acquire_repository(&self, repository: RepositoryId) -> Result<LeaseGuard, LeaseError> {
        self.acquire(LeaseScope::Repository { repository })
    }

    /// Acquires every lease required before a phase begins, then invokes `phase`.
    ///
    /// If either acquisition fails, `phase` is never called. Tree-mutating phases
    /// take the repository/train lease first and the repository lease second, so
    /// concurrent trains cannot overlap a working-tree mutation.
    pub fn with_phase_leases<T>(
        &self,
        repository: RepositoryId,
        train: TrainId,
        tree_mutating: bool,
        phase: impl FnOnce() -> T,
    ) -> Result<T, LeaseError> {
        let train_lease = self.acquire_train(repository.clone(), train)?;
        let repository_lease = if tree_mutating {
            Some(self.acquire_repository(repository)?)
        } else {
            None
        };
        let result = phase();
        if let Some(lease) = repository_lease {
            lease.release()?;
        }
        train_lease.release()?;
        Ok(result)
    }

    /// Returns the durable path for a given lease scope.
    pub fn path_for(&self, scope: &LeaseScope) -> Result<PathBuf, LeaseError> {
        let repository = match scope {
            LeaseScope::RepositoryTrain { repository, .. }
            | LeaseScope::Repository { repository } => repository,
        };
        validate_path_component(repository.as_str())?;
        let repository_dir = self.state_root.join(repository.as_str());
        let leases_dir = repository_dir.join("leases");
        match scope {
            LeaseScope::RepositoryTrain { train, .. } => {
                validate_path_component(train.as_str())?;
                Ok(leases_dir.join(format!("train-{}.lease", train.as_str())))
            }
            LeaseScope::Repository { .. } => Ok(leases_dir.join("repository.lease")),
        }
    }

    fn acquire(&self, scope: LeaseScope) -> Result<LeaseGuard, LeaseError> {
        let path = self.path_for(&scope)?;
        let parent = path
            .parent()
            .expect("a lease path always includes a parent directory");
        fs::create_dir_all(parent).map_err(|source| LeaseError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        sync_directory(parent)?;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Deliberately not truncating at open: the current holder's record
            // must survive until try_lock decides ownership. write_holder
            // truncates after the lock is held.
            .truncate(false)
            .open(&path)
            .map_err(|source| lease_io(&path, source))?;
        if let Err(source) = FileExt::try_lock(&file) {
            return Err(lock_failure(&path, scope, source));
        }

        let holder = LeaseHolder {
            process_id: process::id(),
            acquired_at_ms: timestamp_millis()?,
        };
        write_holder(&mut file, &path, &holder)?;
        sync_directory(parent)?;
        Ok(LeaseGuard {
            file: Some(file),
            path,
            scope,
        })
    }
}

/// A held exclusive lease. Call [`Self::release`] to surface unlock failures.
pub struct LeaseGuard {
    file: Option<File>,
    path: PathBuf,
    scope: LeaseScope,
}

impl LeaseGuard {
    /// Returns the held lease scope.
    pub fn scope(&self) -> &LeaseScope {
        &self.scope
    }

    /// Releases this operating-system lock and reports failure rather than weakening the guarantee.
    pub fn release(mut self) -> Result<(), LeaseError> {
        let file = self
            .file
            .take()
            .expect("a LeaseGuard releases its file at most once");
        FileExt::unlock(&file).map_err(|source| LeaseError::UnsupportedLocking {
            path: self.path.clone(),
            source,
        })
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

fn write_holder(file: &mut File, path: &Path, holder: &LeaseHolder) -> Result<(), LeaseError> {
    let bytes = serde_json::to_vec(holder).map_err(|error| LeaseError::CorruptHolder {
        path: path.to_path_buf(),
        reason: format!("could not encode holder: {error}"),
    })?;
    file.set_len(0).map_err(|source| lease_io(path, source))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| lease_io(path, source))?;
    file.write_all(&bytes)
        .map_err(|source| lease_io(path, source))?;
    file.sync_all()
        .map_err(|source| LeaseError::UnsupportedDurability {
            path: path.to_path_buf(),
            source,
        })
}

fn lock_failure(path: &Path, scope: LeaseScope, source: TryLockError) -> LeaseError {
    match source {
        TryLockError::WouldBlock => LeaseError::Conflict {
            scope,
            holder: read_holder(path).ok(),
        },
        TryLockError::Error(source) => LeaseError::UnsupportedLocking {
            path: path.to_path_buf(),
            source,
        },
    }
}

fn read_holder(path: &Path) -> Result<LeaseHolder, LeaseError> {
    let mut file = File::open(path).map_err(|source| lease_io(path, source))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| lease_io(path, source))?;
    serde_json::from_str(&contents).map_err(|error| LeaseError::CorruptHolder {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

fn timestamp_millis() -> Result<u128, LeaseError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| LeaseError::CorruptHolder {
            path: PathBuf::new(),
            reason: format!("system clock predates the Unix epoch: {error}"),
        })
}

fn sync_directory(path: &Path) -> Result<(), LeaseError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| LeaseError::UnsupportedDurability {
            path: path.to_path_buf(),
            source,
        })
}

fn lease_io(path: &Path, source: io::Error) -> LeaseError {
    LeaseError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn validate_path_component(value: &str) -> Result<(), LeaseError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(LeaseError::UnsafeIdentity(value.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn repository() -> RepositoryId {
        RepositoryId::new("example-repository")
    }

    fn train(name: &str) -> TrainId {
        TrainId::new(name)
    }

    #[test]
    fn conflicting_train_lease_refuses_before_the_affected_phase_begins() {
        let root = tempdir().unwrap();
        let first = LeaseStore::new(root.path()).unwrap();
        let second = LeaseStore::new(root.path()).unwrap();
        let _held = first.acquire_train(repository(), train("release")).unwrap();
        let mut phase_started = false;

        let error = second
            .with_phase_leases(repository(), train("release"), false, || {
                phase_started = true;
            })
            .unwrap_err();

        assert!(!phase_started);
        assert!(matches!(
            error,
            LeaseError::Conflict {
                scope: LeaseScope::RepositoryTrain { .. },
                holder: Some(LeaseHolder { process_id, .. }),
            } if process_id == process::id()
        ));
    }

    #[test]
    fn tree_mutating_phase_also_requires_the_repository_lease_before_starting() {
        let root = tempdir().unwrap();
        let first = LeaseStore::new(root.path()).unwrap();
        let second = LeaseStore::new(root.path()).unwrap();
        let _held = first.acquire_repository(repository()).unwrap();
        let mut phase_started = false;

        let error = second
            .with_phase_leases(repository(), train("other-train"), true, || {
                phase_started = true;
            })
            .unwrap_err();

        assert!(!phase_started);
        assert!(matches!(
            error,
            LeaseError::Conflict {
                scope: LeaseScope::Repository { .. },
                ..
            }
        ));
    }

    #[test]
    fn locking_and_durability_failures_are_typed_refusals_not_fallbacks() {
        let scope = LeaseScope::Repository {
            repository: repository(),
        };
        let locking = lock_failure(
            Path::new("/unsupported/lease"),
            scope,
            io::Error::new(io::ErrorKind::Unsupported, "locking is unavailable").into(),
        );
        assert!(matches!(locking, LeaseError::UnsupportedLocking { .. }));

        let durability = sync_directory(Path::new("/path-that-does-not-exist"));
        assert!(matches!(
            durability,
            Err(LeaseError::UnsupportedDurability { .. })
        ));
    }
}
