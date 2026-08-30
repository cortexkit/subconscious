//! Durable approval bindings for the first public release effect.
//!
//! An approval is valid only while its complete subject still equals the current
//! dry-run plan. The subject includes the ordered effects rather than a single
//! phase name so an approval cannot silently expand to later public work.

use crate::{
    plan::{PublicEffect, ReleaseIdentity, ReleasePlan},
    state::TrainJournalIdentity,
    ApprovalToken, ArtifactId, CommitId, DeclarationDigest, RepositoryId, TrainId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// The persisted approval format accepted by this release-machine version.
pub const APPROVAL_FORMAT_VERSION: u32 = 1;

/// One artifact binding in declaration order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovedArtifact {
    pub artifact: ArtifactId,
    pub identity: String,
    pub digest: String,
}

/// The complete, immutable input an operator confirms at the first public trigger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalSubject {
    pub repository: RepositoryId,
    pub train: TrainId,
    pub intended_commit: CommitId,
    pub declaration_digest: DeclarationDigest,
    pub artifacts: Vec<ApprovedArtifact>,
    pub version_or_run_id: ReleaseIdentity,
    pub public_effects: Vec<PublicEffect>,
}

/// A durable approval token bound to exactly one subject.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalRecord {
    pub subject: ApprovalSubject,
    pub token: ApprovalToken,
}

/// Fail-closed approval construction and durable-state errors.
#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("train `{train}` has no public trigger and cannot request approval")]
    NoPublicTrigger { train: String },
    #[error("train `{train}` has an inconsistent first public trigger")]
    InconsistentPublicTrigger { train: String },
    #[error("approval subject does not belong to this repository and train")]
    SubjectDoesNotMatchStore,
    #[error("no durable approval exists for the current plan")]
    NoCurrentApproval,
    #[error("durable approval does not match the current plan; fresh approval is required")]
    SubjectMismatch,
    #[error("unsupported approval format version {version}")]
    UnsupportedFormatVersion { version: u32 },
    #[error("approval record is corrupt: {reason}")]
    CorruptRecord { reason: String },
    #[error("unable to access approval state at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("filesystem at {path} cannot provide the required approval durability: {source}")]
    UnsupportedDurability {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Builds the single approval subject for a plan's first public trigger.
pub fn build_approval_subject(plan: &ReleasePlan) -> Result<ApprovalSubject, ApprovalError> {
    let Some(first_effect) = plan.public_effects.first() else {
        return Err(ApprovalError::NoPublicTrigger {
            train: plan.train.to_string(),
        });
    };
    if plan.first_public_trigger.as_ref() != Some(first_effect) {
        return Err(ApprovalError::InconsistentPublicTrigger {
            train: plan.train.to_string(),
        });
    }

    Ok(ApprovalSubject {
        repository: plan.repository.clone(),
        train: plan.train.clone(),
        intended_commit: plan.intended_commit.clone(),
        declaration_digest: plan.declaration_digest.clone(),
        artifacts: plan
            .artifacts
            .iter()
            .map(|artifact| ApprovedArtifact {
                artifact: artifact.artifact.clone(),
                identity: artifact.identity.clone(),
                digest: artifact.digest.clone(),
            })
            .collect(),
        version_or_run_id: plan.release_identity.clone(),
        public_effects: plan.public_effects.clone(),
    })
}

/// Stores approval records outside the working repository and synchronizes every update.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    state_root: PathBuf,
    identity: TrainJournalIdentity,
}

impl ApprovalStore {
    /// Opens approval storage below a caller-selected durable-state root.
    pub fn new(
        state_root: impl Into<PathBuf>,
        identity: TrainJournalIdentity,
    ) -> Result<Self, ApprovalError> {
        let store = Self {
            state_root: state_root.into(),
            identity,
        };
        fs::create_dir_all(store.repository_dir()).map_err(|source| ApprovalError::Io {
            path: store.repository_dir(),
            source,
        })?;
        sync_directory(&store.repository_dir())?;
        Ok(store)
    }

    /// Returns the approval file for this repository/train identity.
    pub fn approval_path(&self) -> PathBuf {
        self.repository_dir()
            .join(format!("{}.approval", self.identity.file_stem()))
    }

    /// Reads the durable approval, if one exists, and verifies its format and checksum.
    pub fn load(&self) -> Result<Option<ApprovalRecord>, ApprovalError> {
        let path = self.approval_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(approval_io(&path, source)),
        };
        let envelope: ApprovalEnvelope =
            serde_json::from_slice(&bytes).map_err(|error| ApprovalError::CorruptRecord {
                reason: error.to_string(),
            })?;
        if envelope.version != APPROVAL_FORMAT_VERSION {
            return Err(ApprovalError::UnsupportedFormatVersion {
                version: envelope.version,
            });
        }
        let encoded_record =
            serde_json::to_vec(&envelope.record).map_err(|error| ApprovalError::CorruptRecord {
                reason: format!("could not re-encode approval record: {error}"),
            })?;
        if envelope.checksum != checksum(&encoded_record) {
            return Err(ApprovalError::CorruptRecord {
                reason: "record checksum does not match".to_owned(),
            });
        }
        Ok(Some(envelope.record))
    }

    /// Returns the approval only when it exactly matches the current plan subject.
    pub fn require_current(
        &self,
        current_subject: &ApprovalSubject,
    ) -> Result<ApprovalRecord, ApprovalError> {
        let record = self.load()?.ok_or(ApprovalError::NoCurrentApproval)?;
        if record.subject != *current_subject {
            return Err(ApprovalError::SubjectMismatch);
        }
        Ok(record)
    }

    /// Removes an approval whose complete subject is stale for the current plan.
    ///
    /// Call this whenever a plan is rebuilt before requesting a fresh approval.
    /// The equality comparison covers commit, declaration, artifact bytes, ordered
    /// effect list, version-or-run-id, and tag identity through the plan subject.
    pub fn invalidate_if_stale(
        &self,
        current_subject: &ApprovalSubject,
    ) -> Result<bool, ApprovalError> {
        let Some(record) = self.load()? else {
            return Ok(false);
        };
        if record.subject == *current_subject {
            return Ok(false);
        }

        let path = self.approval_path();
        fs::remove_file(&path).map_err(|source| approval_io(&path, source))?;
        sync_directory(&self.repository_dir())?;
        Ok(true)
    }

    /// Persists a confirmed approval before any public executor may be invoked.
    ///
    /// Replacing an older record is safe only because callers must use
    /// [`Self::invalidate_if_stale`] or [`Self::require_current`] against the
    /// newly constructed subject before they admit a public effect.
    pub fn persist_confirmed(
        &self,
        subject: ApprovalSubject,
        token: ApprovalToken,
    ) -> Result<ApprovalRecord, ApprovalError> {
        if subject.repository != self.identity.repository || subject.train != self.identity.train {
            return Err(ApprovalError::SubjectDoesNotMatchStore);
        }
        let record = ApprovalRecord { subject, token };
        self.write_record(&record)?;
        Ok(record)
    }

    fn write_record(&self, record: &ApprovalRecord) -> Result<(), ApprovalError> {
        let path = self.approval_path();
        let record_bytes =
            serde_json::to_vec(record).map_err(|error| ApprovalError::CorruptRecord {
                reason: format!("could not encode approval record: {error}"),
            })?;
        let envelope = ApprovalEnvelope {
            version: APPROVAL_FORMAT_VERSION,
            record: record.clone(),
            checksum: checksum(&record_bytes),
        };
        let bytes =
            serde_json::to_vec(&envelope).map_err(|error| ApprovalError::CorruptRecord {
                reason: format!("could not encode approval envelope: {error}"),
            })?;
        let parent = self.repository_dir();
        let temporary = parent.join(format!(
            ".{}.{}-{}",
            self.identity.file_stem(),
            process::id(),
            timestamp_millis()?
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| approval_io(&temporary, source))?;
        file.write_all(&bytes)
            .map_err(|source| approval_io(&temporary, source))?;
        file.sync_all()
            .map_err(|source| ApprovalError::UnsupportedDurability {
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, &path).map_err(|source| approval_io(&path, source))?;
        sync_directory(&parent)
    }

    fn repository_dir(&self) -> PathBuf {
        self.state_root.join(self.identity.repository.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApprovalEnvelope {
    version: u32,
    record: ApprovalRecord,
    checksum: String,
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn timestamp_millis() -> Result<u128, ApprovalError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| ApprovalError::CorruptRecord {
            reason: format!("system clock predates the Unix epoch: {error}"),
        })
}

fn sync_directory(path: &Path) -> Result<(), ApprovalError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ApprovalError::UnsupportedDurability {
            path: path.to_path_buf(),
            source,
        })
}

fn approval_io(path: &Path, source: io::Error) -> ApprovalError {
    ApprovalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        declaration::parse,
        plan::{build_dry_run_plan, FinalizedArtifact},
        ArtifactId,
    };
    use tempfile::tempdir;

    fn declaration(tag: &str) -> String {
        format!(
            r#"{{
              "version": 1,
              "trains": [{{
                "id": "release",
                "intended_commit": "abc123",
                "tag": "{tag}",
                "signing_profile": "none",
                "operator_gates": ["first_public_trigger"],
                "artifacts": [{{"id": "archive", "kind": "archive", "identity_channel": "asset_sha256"}}],
                "phases": [
                  {{"id": "tag", "type": "tag"}},
                  {{"id": "publish", "type": "publish"}}
                ]
              }}]
            }}"#
        )
    }

    fn plan(tag: &str) -> ReleasePlan {
        let declaration = parse(&declaration(tag)).unwrap();
        build_dry_run_plan(
            RepositoryId::new("example-repository"),
            &declaration,
            "release",
            &[FinalizedArtifact {
                artifact: ArtifactId::new("archive"),
                identity: format!("archive-{tag}"),
                bytes: b"final archive bytes".to_vec(),
            }],
        )
        .unwrap()
    }

    fn store() -> (tempfile::TempDir, ApprovalStore) {
        let root = tempdir().unwrap();
        let identity = TrainJournalIdentity::new(
            RepositoryId::new("example-repository"),
            TrainId::new("release"),
            "v1.2.3",
        )
        .unwrap();
        let store = ApprovalStore::new(root.path(), identity).unwrap();
        (root, store)
    }

    #[test]
    fn subject_at_first_public_trigger_binds_complete_ordered_release_plan() {
        let plan = plan("v1.2.3");
        let subject = build_approval_subject(&plan).unwrap();

        assert_eq!(subject.repository, RepositoryId::new("example-repository"));
        assert_eq!(subject.train, TrainId::new("release"));
        assert_eq!(subject.intended_commit, CommitId::new("abc123"));
        assert_eq!(subject.declaration_digest, plan.declaration_digest);
        assert_eq!(
            subject.version_or_run_id,
            ReleaseIdentity::Version("v1.2.3".to_owned())
        );
        assert_eq!(
            subject
                .artifacts
                .iter()
                .map(|artifact| artifact.artifact.as_str())
                .collect::<Vec<_>>(),
            ["archive"]
        );
        assert_eq!(
            subject
                .public_effects
                .iter()
                .map(|effect| effect.operation.as_str())
                .collect::<Vec<_>>(),
            ["tag:v1.2.3", "publish:archive"]
        );
        assert_eq!(
            subject.public_effects[0],
            plan.first_public_trigger.unwrap()
        );
    }

    #[test]
    fn approval_is_durable_before_use_and_retagging_invalidates_the_old_subject() {
        let (_root, store) = store();
        let original = build_approval_subject(&plan("v1.2.3")).unwrap();
        store
            .persist_confirmed(original.clone(), ApprovalToken::new("confirmed-v1.2.3"))
            .unwrap();
        assert_eq!(store.load().unwrap().unwrap().subject, original);
        assert!(store.approval_path().exists());

        let retagged = build_approval_subject(&plan("v1.2.4")).unwrap();
        assert!(matches!(
            store.require_current(&retagged),
            Err(ApprovalError::SubjectMismatch)
        ));
        assert!(store.invalidate_if_stale(&retagged).unwrap());
        assert!(matches!(
            store.require_current(&retagged),
            Err(ApprovalError::NoCurrentApproval)
        ));
        assert!(!store.approval_path().exists());
    }
}
