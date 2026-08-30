//! Irreversible artifact publication and staging seams.
//!
//! Executor implementations are traits so the release machine can prove exact
//! calls in tests without reaching a registry, release service, or staging host.

use crate::{
    approval::{ApprovalSubject, ApprovedArtifact},
    artifact::{
        verify_provider_evidence, ArtifactError, ArtifactReleaseIdentity, FinalizedArtifact,
        ProviderArtifactEvidence,
    },
    plan::{PublicEffect, ReleaseIdentity},
    ArtifactId, EffectRequest,
};
use std::collections::VecDeque;
use thiserror::Error;

/// The exact final artifact and approval binding admitted for one publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationRequest {
    pub effect: EffectRequest,
    pub artifact: FinalizedArtifact,
    pub approval: ApprovalSubject,
}

impl PublicationRequest {
    /// Admits a request only when the approval covers the exact bytes to publish.
    pub fn new(
        effect: EffectRequest,
        artifact: FinalizedArtifact,
        approval: ApprovalSubject,
    ) -> Result<Self, ExecutorError> {
        let request = Self {
            effect,
            artifact,
            approval,
        };
        request.validate_approval_binding()?;
        Ok(request)
    }

    /// Rechecks the approval-to-byte binding before invoking an executor.
    pub fn validate_approval_binding(&self) -> Result<(), ExecutorError> {
        self.artifact.verify_digest()?;
        let artifact_id = self.artifact.identity().artifact();
        if self.effect.artifact != *artifact_id {
            return Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "effect artifact `{}` does not match finalized artifact `{artifact_id}`",
                    self.effect.artifact
                ),
            });
        }
        if self.approval.repository != self.effect.repository
            || self.approval.train != self.effect.train
            || self.approval.intended_commit != self.effect.intended_commit
            || self.approval.declaration_digest != self.effect.declaration_digest
        {
            return Err(ExecutorError::ApprovalBinding {
                reason: "approval subject does not match the publication effect identity"
                    .to_owned(),
            });
        }
        validate_release_binding(
            &self.approval.version_or_run_id,
            self.artifact.identity().release(),
        )?;
        validate_approved_artifact(
            &self.approval.artifacts,
            artifact_id,
            self.artifact.identity().approval_identity().as_str(),
            self.artifact.digest(),
        )?;
        if !self.approval.public_effects.iter().any(|effect| {
            public_effect_matches(
                effect,
                &self.effect.phase,
                &self.effect.operation,
                artifact_id,
            )
        }) {
            return Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "approval subject does not include public operation `{}` for artifact `{artifact_id}`",
                    self.effect.operation
                ),
            });
        }
        Ok(())
    }
}

/// A request to move already-finalized bytes into the selected staging backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageRequest {
    pub artifact: FinalizedArtifact,
}

impl StageRequest {
    /// Requires finalized bytes before a staging backend can receive an artifact.
    pub fn new(artifact: FinalizedArtifact) -> Result<Self, ExecutorError> {
        artifact.verify_digest()?;
        Ok(Self { artifact })
    }
}

/// Evidence that a staging backend stored the expected final artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagingEvidence {
    pub reference: String,
    pub artifact: ArtifactId,
    pub digest: String,
}

/// Performs the irreversible public publication of one artifact.
pub trait PublicationExecutor {
    fn publish(
        &mut self,
        request: &PublicationRequest,
    ) -> Result<ProviderArtifactEvidence, ExecutorError>;
}

/// Performs a staging operation through a replaceable, non-production test seam.
pub trait StagingExecutor {
    fn stage(&mut self, request: &StageRequest) -> Result<StagingEvidence, ExecutorError>;
}

/// Fail-closed executor admission and evidence errors.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ExecutorError {
    #[error("approval binding is invalid: {reason}")]
    ApprovalBinding { reason: String },
    #[error("staging evidence {field} mismatch: expected `{expected}`, observed `{observed}`")]
    StagingEvidenceMismatch {
        field: &'static str,
        expected: String,
        observed: String,
    },
    #[error("executor failed: {0}")]
    Executor(String),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
}

/// Calls the publication seam only after exact approval admission succeeds.
pub fn publish_finalized_artifact(
    executor: &mut impl PublicationExecutor,
    request: &PublicationRequest,
) -> Result<ProviderArtifactEvidence, ExecutorError> {
    request.validate_approval_binding()?;
    let evidence = executor.publish(request)?;
    verify_provider_evidence(request.artifact.identity(), &evidence)?;
    Ok(evidence)
}

/// Calls the staging seam and refuses a receipt for substituted final bytes.
pub fn stage_finalized_artifact(
    executor: &mut impl StagingExecutor,
    request: &StageRequest,
) -> Result<StagingEvidence, ExecutorError> {
    request.artifact.verify_digest()?;
    let evidence = executor.stage(request)?;
    compare_staging_evidence(
        "artifact",
        request.artifact.identity().artifact().as_str(),
        evidence.artifact.as_str(),
    )?;
    compare_staging_evidence("digest", request.artifact.digest(), &evidence.digest)?;
    Ok(evidence)
}

fn validate_release_binding(
    approved: &ReleaseIdentity,
    artifact: &ArtifactReleaseIdentity,
) -> Result<(), ExecutorError> {
    match (approved, artifact) {
        (ReleaseIdentity::Version(approved), ArtifactReleaseIdentity::Tagged { tag })
            if approved == tag =>
        {
            Ok(())
        }
        (ReleaseIdentity::RunId(_), ArtifactReleaseIdentity::NoTag) => Ok(()),
        (ReleaseIdentity::Version(approved), ArtifactReleaseIdentity::Tagged { tag }) => {
            Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "approval release `{approved}` does not match artifact tag `{tag}`"
                ),
            })
        }
        (ReleaseIdentity::Version(approved), ArtifactReleaseIdentity::NoTag) => {
            Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "tagged approval release `{approved}` cannot publish a no-tag artifact"
                ),
            })
        }
        (ReleaseIdentity::RunId(run_id), ArtifactReleaseIdentity::Tagged { tag }) => {
            Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "no-tag approval run `{run_id}` cannot publish tagged artifact `{tag}`"
                ),
            })
        }
    }
}

fn validate_approved_artifact(
    approved_artifacts: &[ApprovedArtifact],
    artifact: &ArtifactId,
    expected_identity: &str,
    expected_digest: &str,
) -> Result<(), ExecutorError> {
    let matching = approved_artifacts
        .iter()
        .filter(|approved| approved.artifact == *artifact)
        .collect::<Vec<_>>();
    let [approved] = matching.as_slice() else {
        return Err(ExecutorError::ApprovalBinding {
            reason: format!(
                "approval must contain exactly one binding for finalized artifact `{artifact}`"
            ),
        });
    };
    if approved.identity != expected_identity {
        return Err(ExecutorError::ApprovalBinding {
            reason: format!(
                "approval identity `{}` does not match finalized identity `{expected_identity}`",
                approved.identity
            ),
        });
    }
    if approved.digest != expected_digest {
        return Err(ExecutorError::ApprovalBinding {
            reason: format!(
                "approval digest `{}` does not describe finalized digest `{expected_digest}`",
                approved.digest
            ),
        });
    }
    Ok(())
}

fn public_effect_matches(
    effect: &PublicEffect,
    phase: &crate::PhaseInstanceId,
    operation: &crate::OperationId,
    artifact: &ArtifactId,
) -> bool {
    effect.phase == *phase
        && effect.operation == *operation
        && effect.artifact.as_ref() == Some(artifact)
}

fn compare_staging_evidence(
    field: &'static str,
    expected: &str,
    observed: &str,
) -> Result<(), ExecutorError> {
    if expected == observed {
        Ok(())
    } else {
        Err(ExecutorError::StagingEvidenceMismatch {
            field,
            expected: expected.to_owned(),
            observed: observed.to_owned(),
        })
    }
}

/// Exact calls captured by [`RecordingArtifactExecutor`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordedExecutorCall {
    Publish(Box<PublicationRequest>),
    Stage(Box<StageRequest>),
}

/// A side-effect-free fake for publication and staging call-order assertions.
pub struct RecordingArtifactExecutor {
    calls: Vec<RecordedExecutorCall>,
    publication_outcomes: VecDeque<Result<ProviderArtifactEvidence, ExecutorError>>,
    staging_outcomes: VecDeque<Result<StagingEvidence, ExecutorError>>,
}

impl RecordingArtifactExecutor {
    pub fn new(
        publication_outcomes: impl IntoIterator<Item = Result<ProviderArtifactEvidence, ExecutorError>>,
        staging_outcomes: impl IntoIterator<Item = Result<StagingEvidence, ExecutorError>>,
    ) -> Self {
        Self {
            calls: Vec::new(),
            publication_outcomes: publication_outcomes.into_iter().collect(),
            staging_outcomes: staging_outcomes.into_iter().collect(),
        }
    }

    pub fn calls(&self) -> &[RecordedExecutorCall] {
        &self.calls
    }
}

impl PublicationExecutor for RecordingArtifactExecutor {
    fn publish(
        &mut self,
        request: &PublicationRequest,
    ) -> Result<ProviderArtifactEvidence, ExecutorError> {
        self.calls
            .push(RecordedExecutorCall::Publish(Box::new(request.clone())));
        self.publication_outcomes.pop_front().unwrap_or_else(|| {
            Err(ExecutorError::Executor(
                "no publication outcome was scripted".to_owned(),
            ))
        })
    }
}

impl StagingExecutor for RecordingArtifactExecutor {
    fn stage(&mut self, request: &StageRequest) -> Result<StagingEvidence, ExecutorError> {
        self.calls
            .push(RecordedExecutorCall::Stage(Box::new(request.clone())));
        self.staging_outcomes.pop_front().unwrap_or_else(|| {
            Err(ExecutorError::Executor(
                "no staging outcome was scripted".to_owned(),
            ))
        })
    }
}
