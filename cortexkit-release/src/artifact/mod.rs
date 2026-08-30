//! Artifact preparation and provider-identity verification.
//!
//! The values that select identity channels, signing profiles, staging backends,
//! and load classes stay opaque here. Their closed vocabularies belong to the
//! immutable [`crate::NORMATIVE_SPEC_REFERENCE`], rather than to a second list
//! maintained by this crate.

use crate::{ArtifactId, CommitId, NORMATIVE_SPEC_REFERENCE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Opaque selections whose allowed values are owned by the anchored specification.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSelections {
    pub identity_channel: String,
    pub signing_profile: String,
    pub staging_backend: String,
    pub load_class: String,
}

/// The category of an opaque selection checked against the anchored specification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactSelectionKind {
    IdentityChannel,
    SigningProfile,
    StagingBackend,
    LoadClass,
}

impl ArtifactSelectionKind {
    fn name(self) -> &'static str {
        match self {
            Self::IdentityChannel => "identity channel",
            Self::SigningProfile => "signing profile",
            Self::StagingBackend => "staging backend",
            Self::LoadClass => "load class",
        }
    }
}

/// Resolves whether one opaque selection belongs to the anchored closed set.
///
/// The release machine deliberately receives this authority instead of keeping
/// local lists of allowed values. An authority must evaluate selections against
/// `NORMATIVE_SPEC_REFERENCE`; it must not use repository declaration data as a
/// replacement vocabulary.
pub trait AnchoredSelectionAuthority {
    fn supports(
        &self,
        normative_reference: &str,
        artifact_kind: &str,
        selection_kind: ArtifactSelectionKind,
        value: &str,
    ) -> bool;
}

/// The release identity against which provider evidence is checked.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactReleaseIdentity {
    Tagged { tag: String },
    NoTag,
}

/// The fully resolved identity of one artifact before it is prepared.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    artifact: ArtifactId,
    kind: String,
    release: ArtifactReleaseIdentity,
    intended_commit: CommitId,
    selections: ArtifactSelections,
}

impl ArtifactIdentity {
    /// Creates an identity only after every selection is accepted by the anchor.
    pub fn new(
        artifact: ArtifactId,
        kind: impl Into<String>,
        release: ArtifactReleaseIdentity,
        intended_commit: CommitId,
        selections: ArtifactSelections,
        authority: &impl AnchoredSelectionAuthority,
    ) -> Result<Self, ArtifactError> {
        let kind = kind.into();
        if kind.trim().is_empty() {
            return Err(ArtifactError::MissingArtifactKind {
                artifact: artifact.to_string(),
            });
        }
        if selections.identity_channel.trim().is_empty() {
            return Err(ArtifactError::MissingIdentityChannel {
                artifact: artifact.to_string(),
                kind,
            });
        }

        for (selection_kind, value) in [
            (
                ArtifactSelectionKind::IdentityChannel,
                &selections.identity_channel,
            ),
            (
                ArtifactSelectionKind::SigningProfile,
                &selections.signing_profile,
            ),
            (
                ArtifactSelectionKind::StagingBackend,
                &selections.staging_backend,
            ),
            (ArtifactSelectionKind::LoadClass, &selections.load_class),
        ] {
            if value.trim().is_empty()
                || !authority.supports(NORMATIVE_SPEC_REFERENCE, &kind, selection_kind, value)
            {
                return Err(ArtifactError::UnsupportedSelection {
                    artifact: artifact.to_string(),
                    selection: selection_kind.name(),
                    value: value.clone(),
                });
            }
        }

        Ok(Self {
            artifact,
            kind,
            release,
            intended_commit,
            selections,
        })
    }

    pub fn artifact(&self) -> &ArtifactId {
        &self.artifact
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn release(&self) -> &ArtifactReleaseIdentity {
        &self.release
    }

    pub fn intended_commit(&self) -> &CommitId {
        &self.intended_commit
    }

    pub fn selections(&self) -> &ArtifactSelections {
        &self.selections
    }

    /// Returns the identity string that approval persists alongside the digest.
    pub fn approval_identity(&self) -> String {
        match &self.release {
            ArtifactReleaseIdentity::Tagged { tag } => tag.clone(),
            ArtifactReleaseIdentity::NoTag => self.intended_commit.to_string(),
        }
    }
}

/// Whether the input is a raw asset needing pre-sign verification or build output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPath {
    Raw,
    Transformed,
}

/// Artifact bytes and identity available before the anchored preparation pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactInput {
    pub identity: ArtifactIdentity,
    pub path: ArtifactPath,
    pub bytes: Vec<u8>,
}

/// Final artifact material whose digest is safe to put into an approval subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedArtifact {
    identity: ArtifactIdentity,
    bytes: Vec<u8>,
    digest: String,
    sidecar: Vec<u8>,
}

impl FinalizedArtifact {
    pub fn identity(&self) -> &ArtifactIdentity {
        &self.identity
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn sidecar(&self) -> &[u8] {
        &self.sidecar
    }

    /// Refuses stale or substituted bytes before they can be passed to an executor.
    pub fn verify_digest(&self) -> Result<(), ArtifactError> {
        let actual = artifact_digest(&self.bytes);
        if actual == self.digest {
            Ok(())
        } else {
            Err(ArtifactError::DigestMismatch {
                artifact: self.identity.artifact.to_string(),
                expected: self.digest.clone(),
                observed: actual,
            })
        }
    }
}

/// Evidence returned by a provider after it has observed a published artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderArtifactEvidence {
    pub reference: String,
    pub artifact: ArtifactId,
    pub commit: CommitId,
    pub release: Option<String>,
    pub embedded_build_sha: Option<CommitId>,
}

/// Fail-closed errors from artifact selection, preparation, and evidence checking.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ArtifactError {
    #[error("artifact `{artifact}` has no kind")]
    MissingArtifactKind { artifact: String },
    #[error("artifact `{artifact}` of kind `{kind}` has no identity channel")]
    MissingIdentityChannel { artifact: String, kind: String },
    #[error("artifact `{artifact}` selects unsupported {selection} `{value}`")]
    UnsupportedSelection {
        artifact: String,
        selection: &'static str,
        value: String,
    },
    #[error("artifact transformer failed during {step}: {message}")]
    Transformer { step: &'static str, message: String },
    #[error("artifact `{artifact}` digest mismatch: expected `{expected}`, observed `{observed}`")]
    DigestMismatch {
        artifact: String,
        expected: String,
        observed: String,
    },
    #[error("provider evidence {field} mismatch: expected `{expected}`, observed `{observed}`")]
    ProviderEvidenceMismatch {
        field: &'static str,
        expected: String,
        observed: String,
    },
    #[error("no-tag artifact `{artifact}` is missing compiler-emitted build-SHA evidence")]
    MissingEmbeddedBuildSha { artifact: String },
}

/// Operations needed to finalize artifact bytes without a production side effect.
///
/// The order is owned by [`finalize_artifact`]. Implementations cannot move a
/// sidecar ahead of signing because the sidecar receives the signed bytes, and
/// raw inputs are verified before the machine asks for a replacement signature.
pub trait ArtifactTransformer {
    fn verify_raw(&mut self, input: &ArtifactInput) -> Result<(), ArtifactError>;

    fn strip(&mut self, input: &ArtifactInput, bytes: &[u8]) -> Result<Vec<u8>, ArtifactError>;

    fn sign(&mut self, input: &ArtifactInput, bytes: &[u8]) -> Result<Vec<u8>, ArtifactError>;

    fn sidecar_from_signed_bytes(
        &mut self,
        input: &ArtifactInput,
        signed_bytes: &[u8],
    ) -> Result<Vec<u8>, ArtifactError>;

    fn verify_readback(
        &mut self,
        input: &ArtifactInput,
        signed_bytes: &[u8],
        sidecar: &[u8],
    ) -> Result<(), ArtifactError>;
}

/// Finalizes bytes in the anchored order before any approval can be constructed.
pub fn finalize_artifact(
    transformer: &mut impl ArtifactTransformer,
    input: ArtifactInput,
) -> Result<FinalizedArtifact, ArtifactError> {
    if input.path == ArtifactPath::Raw {
        transformer.verify_raw(&input)?;
    }
    let stripped = transformer.strip(&input, &input.bytes)?;
    let signed = transformer.sign(&input, &stripped)?;
    let sidecar = transformer.sidecar_from_signed_bytes(&input, &signed)?;
    transformer.verify_readback(&input, &signed, &sidecar)?;
    let digest = artifact_digest(&signed);

    Ok(FinalizedArtifact {
        identity: input.identity,
        bytes: signed,
        digest,
        sidecar,
    })
}

/// Verifies that provider evidence identifies this exact artifact and release.
pub fn verify_provider_evidence(
    identity: &ArtifactIdentity,
    evidence: &ProviderArtifactEvidence,
) -> Result<(), ArtifactError> {
    compare_evidence(
        "artifact",
        identity.artifact.as_str(),
        evidence.artifact.as_str(),
    )?;
    compare_evidence(
        "commit",
        identity.intended_commit.as_str(),
        evidence.commit.as_str(),
    )?;

    match &identity.release {
        ArtifactReleaseIdentity::Tagged { tag } => match &evidence.release {
            Some(observed) => compare_evidence("release", tag, observed),
            None => Err(ArtifactError::ProviderEvidenceMismatch {
                field: "release",
                expected: tag.clone(),
                observed: "<missing>".to_owned(),
            }),
        },
        ArtifactReleaseIdentity::NoTag => {
            if let Some(observed) = &evidence.release {
                return Err(ArtifactError::ProviderEvidenceMismatch {
                    field: "release",
                    expected: "<no tag>".to_owned(),
                    observed: observed.clone(),
                });
            }
            let Some(build_sha) = &evidence.embedded_build_sha else {
                return Err(ArtifactError::MissingEmbeddedBuildSha {
                    artifact: identity.artifact.to_string(),
                });
            };
            compare_evidence(
                "embedded build SHA",
                identity.intended_commit.as_str(),
                build_sha.as_str(),
            )?;
            Ok(())
        }
    }
}

/// Computes the digest approval and publication must share.
pub fn artifact_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn compare_evidence(
    field: &'static str,
    expected: &str,
    observed: &str,
) -> Result<(), ArtifactError> {
    if expected == observed {
        Ok(())
    } else {
        Err(ArtifactError::ProviderEvidenceMismatch {
            field,
            expected: expected.to_owned(),
            observed: observed.to_owned(),
        })
    }
}

/// Calls recorded by [`RecordingArtifactTransformer`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordedArtifactCall {
    VerifyRaw(ArtifactInput),
    Strip {
        artifact: ArtifactId,
        bytes: Vec<u8>,
    },
    Sign {
        artifact: ArtifactId,
        bytes: Vec<u8>,
    },
    SidecarFromSignedBytes {
        artifact: ArtifactId,
        bytes: Vec<u8>,
    },
    VerifyReadback {
        artifact: ArtifactId,
        signed_bytes: Vec<u8>,
        sidecar: Vec<u8>,
    },
}

/// A side-effect-free transformer fake for exact pipeline-order assertions.
pub struct RecordingArtifactTransformer {
    calls: Vec<RecordedArtifactCall>,
    stripped_bytes: Vec<u8>,
    signed_bytes: Vec<u8>,
    sidecar: Vec<u8>,
}

impl RecordingArtifactTransformer {
    pub fn new(
        stripped_bytes: impl Into<Vec<u8>>,
        signed_bytes: impl Into<Vec<u8>>,
        sidecar: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            calls: Vec::new(),
            stripped_bytes: stripped_bytes.into(),
            signed_bytes: signed_bytes.into(),
            sidecar: sidecar.into(),
        }
    }

    pub fn calls(&self) -> &[RecordedArtifactCall] {
        &self.calls
    }
}

impl ArtifactTransformer for RecordingArtifactTransformer {
    fn verify_raw(&mut self, input: &ArtifactInput) -> Result<(), ArtifactError> {
        self.calls
            .push(RecordedArtifactCall::VerifyRaw(input.clone()));
        Ok(())
    }

    fn strip(&mut self, input: &ArtifactInput, bytes: &[u8]) -> Result<Vec<u8>, ArtifactError> {
        self.calls.push(RecordedArtifactCall::Strip {
            artifact: input.identity.artifact.clone(),
            bytes: bytes.to_vec(),
        });
        Ok(self.stripped_bytes.clone())
    }

    fn sign(&mut self, input: &ArtifactInput, bytes: &[u8]) -> Result<Vec<u8>, ArtifactError> {
        self.calls.push(RecordedArtifactCall::Sign {
            artifact: input.identity.artifact.clone(),
            bytes: bytes.to_vec(),
        });
        Ok(self.signed_bytes.clone())
    }

    fn sidecar_from_signed_bytes(
        &mut self,
        input: &ArtifactInput,
        signed_bytes: &[u8],
    ) -> Result<Vec<u8>, ArtifactError> {
        self.calls
            .push(RecordedArtifactCall::SidecarFromSignedBytes {
                artifact: input.identity.artifact.clone(),
                bytes: signed_bytes.to_vec(),
            });
        Ok(self.sidecar.clone())
    }

    fn verify_readback(
        &mut self,
        input: &ArtifactInput,
        signed_bytes: &[u8],
        sidecar: &[u8],
    ) -> Result<(), ArtifactError> {
        self.calls.push(RecordedArtifactCall::VerifyReadback {
            artifact: input.identity.artifact.clone(),
            signed_bytes: signed_bytes.to_vec(),
            sidecar: sidecar.to_vec(),
        });
        Ok(())
    }
}
