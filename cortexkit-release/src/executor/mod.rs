//! Irreversible artifact publication and staging seams.
//!
//! Executor implementations are traits so the release machine can prove exact
//! calls in tests without reaching a registry, release service, or staging host.

use crate::{
    approval::{ApprovalSubject, ApprovedArtifact},
    artifact::{
        artifact_digest, verify_provider_evidence, ArtifactError, ArtifactReleaseIdentity,
        FinalizedArtifact, ProviderArtifactEvidence,
    },
    plan::{FinalizedArtifact as PlannedFinalizedArtifact, PublicEffect, ReleaseIdentity},
    ArtifactDigest, ArtifactId, EffectRequest,
};
use std::collections::VecDeque;
use thiserror::Error;

/// Final artifact material accepted by the shared publication request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationArtifact {
    /// Material finalized through the anchored transformation pipeline.
    Anchored(FinalizedArtifact),
    /// Material retained by the credential-free release plan.
    Planned(PlannedFinalizedArtifact),
}

impl PublicationArtifact {
    pub fn artifact(&self) -> &ArtifactId {
        match self {
            Self::Anchored(artifact) => artifact.identity().artifact(),
            Self::Planned(artifact) => &artifact.artifact,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Anchored(artifact) => artifact.bytes(),
            Self::Planned(artifact) => &artifact.bytes,
        }
    }

    fn approval_identity(&self) -> String {
        match self {
            Self::Anchored(artifact) => artifact.identity().approval_identity(),
            Self::Planned(artifact) => artifact.identity.clone(),
        }
    }

    fn digest(&self) -> String {
        match self {
            Self::Anchored(artifact) => artifact.digest().to_owned(),
            Self::Planned(artifact) => artifact_digest(&artifact.bytes),
        }
    }

    fn validate(&self, approval: &ApprovalSubject) -> Result<(), ExecutorError> {
        match self {
            Self::Anchored(artifact) => {
                artifact.verify_digest()?;
                validate_release_binding(&approval.version_or_run_id, artifact.identity().release())
            }
            Self::Planned(_) => Ok(()),
        }
    }
}

impl From<FinalizedArtifact> for PublicationArtifact {
    fn from(artifact: FinalizedArtifact) -> Self {
        Self::Anchored(artifact)
    }
}

impl From<PlannedFinalizedArtifact> for PublicationArtifact {
    fn from(artifact: PlannedFinalizedArtifact) -> Self {
        Self::Planned(artifact)
    }
}

/// The exact final artifact and approval binding admitted for one publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationRequest {
    pub effect: EffectRequest,
    pub artifact: PublicationArtifact,
    pub approval: ApprovalSubject,
}

impl PublicationRequest {
    /// Admits a request only when the approval covers the exact bytes to publish.
    pub fn new(
        effect: EffectRequest,
        artifact: impl Into<PublicationArtifact>,
        approval: ApprovalSubject,
    ) -> Result<Self, ExecutorError> {
        let request = Self {
            effect,
            artifact: artifact.into(),
            approval,
        };
        request.validate_approval_binding()?;
        Ok(request)
    }

    /// Rechecks the approval-to-byte binding before invoking an executor.
    pub fn validate_approval_binding(&self) -> Result<(), ExecutorError> {
        self.artifact.validate(&self.approval)?;
        let artifact_id = self.artifact.artifact();
        if self.effect.artifact != *artifact_id {
            return Err(ExecutorError::ApprovalBinding {
                reason: format!(
                    "effect artifact `{}` does not match finalized artifact `{artifact_id}`",
                    self.effect.artifact
                ),
            });
        }
        validate_effect_identity(&self.effect, &self.approval)?;
        let approval_identity = self.artifact.approval_identity();
        let digest = self.artifact.digest();
        validate_approved_artifact(
            &self.approval.artifacts,
            artifact_id,
            &approval_identity,
            &digest,
        )?;
        validate_public_effect(
            &self.approval.public_effects,
            &self.effect,
            Some(artifact_id),
        )
    }
}

/// An effect that passed approval checks and is allowed to trigger an irreversible action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedEffect {
    effect: EffectRequest,
    approval: ApprovalSubject,
    publication: Option<PublicationRequest>,
}

impl AdmittedEffect {
    pub(crate) fn new(
        effect: EffectRequest,
        planned_effect: &PublicEffect,
        artifact: Option<PlannedFinalizedArtifact>,
        approval: ApprovalSubject,
    ) -> Result<Self, ExecutorError> {
        validate_effect_identity(&effect, &approval)?;
        validate_public_effect(
            &approval.public_effects,
            &effect,
            planned_effect.artifact.as_ref(),
        )?;
        let publication = match (planned_effect.artifact.as_ref(), artifact) {
            (Some(_), Some(artifact)) => Some(PublicationRequest::new(
                effect.clone(),
                artifact,
                approval.clone(),
            )?),
            (Some(artifact), None) => {
                return Err(ExecutorError::MissingFinalizedArtifact {
                    artifact: artifact.to_string(),
                })
            }
            (None, None) => None,
            (None, Some(artifact)) => {
                return Err(ExecutorError::UnexpectedFinalizedArtifact {
                    artifact: artifact.artifact.to_string(),
                })
            }
        };
        Ok(Self {
            effect,
            approval,
            publication,
        })
    }

    /// Unfenced witness constructor for external seam fakes and acceptance tests.
    /// Production code must obtain this witness through the orchestrator.
    #[doc(hidden)]
    pub fn new_unfenced_for_tests(
        effect: EffectRequest,
        planned_effect: &PublicEffect,
        artifact: Option<PlannedFinalizedArtifact>,
        approval: ApprovalSubject,
    ) -> Result<Self, ExecutorError> {
        Self::new(effect, planned_effect, artifact, approval)
    }

    pub fn effect(&self) -> &EffectRequest {
        &self.effect
    }

    pub fn publication(&self) -> Option<&PublicationRequest> {
        self.publication.as_ref()
    }

    pub fn validate_approval_binding(&self) -> Result<(), ExecutorError> {
        validate_effect_identity(&self.effect, &self.approval)?;
        if let Some(publication) = &self.publication {
            publication.validate_approval_binding()
        } else {
            validate_public_effect(&self.approval.public_effects, &self.effect, None)
        }
    }

    pub(crate) fn durable_approval_subject(&self) -> crate::ApprovalSubject {
        crate::ApprovalSubject {
            repository: self.approval.repository.clone(),
            train: self.approval.train.clone(),
            intended_commit: self.approval.intended_commit.clone(),
            declaration_digest: self.approval.declaration_digest.clone(),
            artifact_digests: self
                .approval
                .artifacts
                .iter()
                .map(|artifact| ArtifactDigest {
                    artifact: artifact.artifact.clone(),
                    digest: artifact.digest.clone(),
                })
                .collect(),
            public_effects: self
                .approval
                .public_effects
                .iter()
                .map(|effect| effect.operation.clone())
                .collect(),
        }
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
    #[error(
        "public effect references finalized artifact `{artifact}` that is absent from the plan"
    )]
    MissingFinalizedArtifact { artifact: String },
    #[error(
        "public effect without an artifact received unexpected finalized artifact `{artifact}`"
    )]
    UnexpectedFinalizedArtifact { artifact: String },
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
    match &request.artifact {
        PublicationArtifact::Anchored(artifact) => {
            verify_provider_evidence(artifact.identity(), &evidence)?;
        }
        PublicationArtifact::Planned(artifact) => {
            if evidence.artifact != artifact.artifact
                || evidence.commit != request.effect.intended_commit
            {
                return Err(ExecutorError::ApprovalBinding {
                    reason: "provider evidence does not match the planned publication identity"
                        .to_owned(),
                });
            }
        }
    }
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

fn validate_effect_identity(
    effect: &EffectRequest,
    approval: &ApprovalSubject,
) -> Result<(), ExecutorError> {
    if approval.repository == effect.repository
        && approval.train == effect.train
        && approval.intended_commit == effect.intended_commit
        && approval.declaration_digest == effect.declaration_digest
    {
        Ok(())
    } else {
        Err(ExecutorError::ApprovalBinding {
            reason: "approval subject does not match the publication effect identity".to_owned(),
        })
    }
}

fn validate_public_effect(
    approved_effects: &[PublicEffect],
    effect: &EffectRequest,
    artifact: Option<&ArtifactId>,
) -> Result<(), ExecutorError> {
    if approved_effects.iter().any(|approved| {
        approved.phase == effect.phase
            && approved.operation == effect.operation
            && approved.artifact.as_ref() == artifact
    }) {
        Ok(())
    } else {
        Err(ExecutorError::ApprovalBinding {
            reason: format!(
                "approval subject does not include public operation `{}` for artifact `{}`",
                effect.operation,
                artifact.map_or("<none>", ArtifactId::as_str)
            ),
        })
    }
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
