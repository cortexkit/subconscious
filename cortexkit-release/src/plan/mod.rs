//! Credential-free release-train planning.
//!
//! Planning consumes only a validated declaration and finalized artifact bytes.
//! It intentionally has no provider or executor seam, so a dry run cannot obtain
//! credentials or cause a public effect.

use crate::{
    declaration::{ParsedDeclaration, PhaseDeclaration, TrainDeclaration},
    ArtifactId, CommitId, DeclarationDigest, OperationId, PhaseInstanceId, RepositoryId, TrainId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// One finalized artifact available to a credential-free plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedArtifact {
    /// The declaration artifact identifier.
    pub artifact: ArtifactId,
    /// The artifact identity carried by the declaration's identity channel.
    pub identity: String,
    /// Final bytes whose digest will be bound to approval.
    pub bytes: Vec<u8>,
}

/// The plan-visible identity used to address a tagged version or a no-tag run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ReleaseIdentity {
    /// A tag or other declared version identity.
    Version(String),
    /// A no-tag run identity derived from the train and intended commit.
    RunId(String),
}

/// A declaration artifact after its final identity and digest are resolved.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedArtifact {
    pub artifact: ArtifactId,
    pub identity_channel: String,
    pub identity: String,
    pub digest: String,
}

/// One phase in declaration order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedPhase {
    pub instance: PhaseInstanceId,
    pub phase_type: String,
    pub tree_mutating: bool,
}

/// A completion probe that must be available for a planned external effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedProbe {
    pub phase: PhaseInstanceId,
    pub artifact: Option<ArtifactId>,
    pub identity_channel: String,
    pub expected_identity: String,
}

/// One planned public effect in the exact declaration and artifact order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicEffect {
    pub phase: PhaseInstanceId,
    pub operation: OperationId,
    pub artifact: Option<ArtifactId>,
}

/// The leases that execution must obtain before it starts the named boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseRequirements {
    /// An exclusive lease for this repository and train is always required.
    pub repository_train: bool,
    /// An exclusive repository lease is required before any listed tree mutation.
    pub tree_mutating_phases: Vec<PhaseInstanceId>,
}

/// Operator-facing instructions emitted after staging has been verified.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlacementInstructions {
    pub terminal_state: String,
    pub instruction: String,
}

/// A complete dry-run plan for one release train.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleasePlan {
    pub repository: RepositoryId,
    pub train: TrainId,
    pub intended_commit: CommitId,
    pub declaration_digest: DeclarationDigest,
    pub release_identity: ReleaseIdentity,
    pub phases: Vec<PlannedPhase>,
    pub artifacts: Vec<PlannedArtifact>,
    pub probes: Vec<PlannedProbe>,
    pub lease_requirements: LeaseRequirements,
    pub first_public_trigger: Option<PublicEffect>,
    pub public_effects: Vec<PublicEffect>,
    pub placement_instructions: PlacementInstructions,
}

/// A typed refusal while resolving a plan without provider access.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PlanError {
    #[error("release declaration has no train named `{0}`")]
    UnknownTrain(String),
    #[error("plan for train `{train}` is missing finalized artifact `{artifact}`")]
    MissingArtifact { train: String, artifact: String },
    #[error("plan for train `{train}` received unexpected artifact `{artifact}`")]
    UnexpectedArtifact { train: String, artifact: String },
    #[error("plan for train `{train}` received artifact `{artifact}` more than once")]
    DuplicateArtifact { train: String, artifact: String },
    #[error("plan for train `{train}` received an empty identity for artifact `{artifact}`")]
    EmptyArtifactIdentity { train: String, artifact: String },
}

/// Builds a complete dry-run plan without credentials, provider calls, or approval.
///
/// `finalized_artifacts` must be produced before planning reaches the first public
/// trigger. Their byte digests are calculated here so approval always binds the
/// exact bytes that the executor is later allowed to publish.
pub fn build_dry_run_plan(
    repository: RepositoryId,
    declaration: &ParsedDeclaration,
    train_name: &str,
    finalized_artifacts: &[FinalizedArtifact],
) -> Result<ReleasePlan, PlanError> {
    let train = declaration
        .declaration
        .trains
        .iter()
        .find(|candidate| candidate.id == train_name)
        .ok_or_else(|| PlanError::UnknownTrain(train_name.to_owned()))?;
    let materials = index_artifacts(train, finalized_artifacts)?;
    let artifacts = resolve_artifacts(train, &materials)?;
    let release_identity = release_identity(train);
    let phases = train
        .phases
        .iter()
        .map(|phase| PlannedPhase {
            instance: phase.instance_id(),
            phase_type: phase.phase_type.clone(),
            tree_mutating: tree_mutating(phase),
        })
        .collect::<Vec<_>>();
    let public_effects = resolve_public_effects(train, &release_identity, &artifacts);
    let probes = resolve_probes(train, &release_identity, &artifacts);
    let tree_mutating_phases = phases
        .iter()
        .filter(|phase| phase.tree_mutating)
        .map(|phase| phase.instance.clone())
        .collect();

    Ok(ReleasePlan {
        repository,
        train: train.train_id(),
        intended_commit: train.intended_commit_id(),
        declaration_digest: declaration.digest.clone(),
        release_identity,
        phases,
        artifacts,
        probes,
        lease_requirements: LeaseRequirements {
            repository_train: true,
            tree_mutating_phases,
        },
        first_public_trigger: public_effects.first().cloned(),
        public_effects,
        placement_instructions: PlacementInstructions {
            terminal_state: "verified_staged_artifacts".to_owned(),
            instruction: "Run the separate operator placement ceremony; ck-release does not place, restart, or mutate the live fleet."
                .to_owned(),
        },
    })
}

fn index_artifacts<'a>(
    train: &TrainDeclaration,
    finalized_artifacts: &'a [FinalizedArtifact],
) -> Result<HashMap<&'a str, &'a FinalizedArtifact>, PlanError> {
    let declared = train
        .artifacts
        .iter()
        .map(|artifact| artifact.id.as_str())
        .collect::<HashSet<_>>();
    let mut materials = HashMap::new();
    for material in finalized_artifacts {
        let artifact = material.artifact.as_str();
        if !declared.contains(artifact) {
            return Err(PlanError::UnexpectedArtifact {
                train: train.id.clone(),
                artifact: artifact.to_owned(),
            });
        }
        if materials.insert(artifact, material).is_some() {
            return Err(PlanError::DuplicateArtifact {
                train: train.id.clone(),
                artifact: artifact.to_owned(),
            });
        }
        if material.identity.trim().is_empty() {
            return Err(PlanError::EmptyArtifactIdentity {
                train: train.id.clone(),
                artifact: artifact.to_owned(),
            });
        }
    }
    Ok(materials)
}

fn resolve_artifacts(
    train: &TrainDeclaration,
    materials: &HashMap<&str, &FinalizedArtifact>,
) -> Result<Vec<PlannedArtifact>, PlanError> {
    train
        .artifacts
        .iter()
        .map(|artifact| {
            let material =
                materials
                    .get(artifact.id.as_str())
                    .ok_or_else(|| PlanError::MissingArtifact {
                        train: train.id.clone(),
                        artifact: artifact.id.clone(),
                    })?;
            Ok(PlannedArtifact {
                artifact: material.artifact.clone(),
                identity_channel: artifact
                    .identity_channel
                    .clone()
                    .expect("validated declarations always have artifact identity channels"),
                identity: material.identity.clone(),
                digest: format!("{:x}", Sha256::digest(&material.bytes)),
            })
        })
        .collect()
}

fn release_identity(train: &TrainDeclaration) -> ReleaseIdentity {
    match &train.tag {
        Some(tag) => ReleaseIdentity::Version(tag.clone()),
        None => ReleaseIdentity::RunId(train.train_key()),
    }
}

fn tree_mutating(phase: &PhaseDeclaration) -> bool {
    matches!(phase.phase_type.as_str(), "stamp" | "tag")
}

fn resolve_public_effects(
    train: &TrainDeclaration,
    release_identity: &ReleaseIdentity,
    artifacts: &[PlannedArtifact],
) -> Vec<PublicEffect> {
    train
        .phases
        .iter()
        .flat_map(|phase| match phase.phase_type.as_str() {
            "tag" => vec![PublicEffect {
                phase: phase.instance_id(),
                operation: OperationId::new(format!(
                    "tag:{}",
                    release_identity_value(release_identity)
                )),
                artifact: None,
            }],
            "publish" | "assets" => artifacts
                .iter()
                .map(|artifact| PublicEffect {
                    phase: phase.instance_id(),
                    operation: OperationId::new(format!(
                        "{}:{}",
                        phase.phase_type, artifact.artifact
                    )),
                    artifact: Some(artifact.artifact.clone()),
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn resolve_probes(
    train: &TrainDeclaration,
    release_identity: &ReleaseIdentity,
    artifacts: &[PlannedArtifact],
) -> Vec<PlannedProbe> {
    train
        .phases
        .iter()
        .flat_map(|phase| match phase.phase_type.as_str() {
            "tag" => vec![PlannedProbe {
                phase: phase.instance_id(),
                artifact: None,
                identity_channel: "tag_at_commit".to_owned(),
                expected_identity: train.intended_commit.clone(),
            }],
            "publish" | "assets" | "stage" => artifacts
                .iter()
                .map(|artifact| PlannedProbe {
                    phase: phase.instance_id(),
                    artifact: Some(artifact.artifact.clone()),
                    identity_channel: artifact.identity_channel.clone(),
                    expected_identity: artifact.identity.clone(),
                })
                .collect(),
            "ci_watch" | "verify_readback" => vec![PlannedProbe {
                phase: phase.instance_id(),
                artifact: None,
                identity_channel: "phase_completion".to_owned(),
                expected_identity: release_identity_value(release_identity).to_owned(),
            }],
            _ => Vec::new(),
        })
        .collect()
}

fn release_identity_value(identity: &ReleaseIdentity) -> &str {
    match identity {
        ReleaseIdentity::Version(value) | ReleaseIdentity::RunId(value) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::parse;

    const DECLARATION: &str = r#"
    {
      "version": 1,
      "trains": [{
        "id": "release",
        "intended_commit": "abc123",
        "tag": "v1.2.3",
        "signing_profile": "none",
        "operator_gates": ["first_public_trigger"],
        "artifacts": [
          {"id": "archive", "kind": "archive", "identity_channel": "asset_sha256"},
          {"id": "crate", "kind": "crate", "identity_channel": "registry_version"}
        ],
        "phases": [
          {"id": "preflight", "type": "preflight"},
          {"id": "stamp", "type": "stamp"},
          {"id": "tag", "type": "tag"},
          {"id": "publish", "type": "publish"},
          {"id": "stage", "type": "stage"}
        ]
      }]
    }
    "#;

    fn finalized_artifacts() -> Vec<FinalizedArtifact> {
        vec![
            FinalizedArtifact {
                artifact: ArtifactId::new("archive"),
                identity: "archive-v1.2.3".to_owned(),
                bytes: b"final archive bytes".to_vec(),
            },
            FinalizedArtifact {
                artifact: ArtifactId::new("crate"),
                identity: "cortexkit-release@1.2.3".to_owned(),
                bytes: b"final crate bytes".to_vec(),
            },
        ]
    }

    #[test]
    fn dry_run_resolves_identity_ordered_phases_probes_leases_and_placement_boundary() {
        let declaration = parse(DECLARATION).unwrap();
        let plan = build_dry_run_plan(
            RepositoryId::new("example-repository"),
            &declaration,
            "release",
            &finalized_artifacts(),
        )
        .unwrap();

        assert_eq!(plan.intended_commit, CommitId::new("abc123"));
        assert_eq!(
            plan.release_identity,
            ReleaseIdentity::Version("v1.2.3".to_owned())
        );
        assert_eq!(
            plan.phases
                .iter()
                .map(|phase| phase.instance.as_str())
                .collect::<Vec<_>>(),
            ["preflight", "stamp", "tag", "publish", "stage"]
        );
        assert_eq!(
            plan.artifacts
                .iter()
                .map(|artifact| artifact.artifact.as_str())
                .collect::<Vec<_>>(),
            ["archive", "crate"]
        );
        assert_eq!(
            plan.public_effects
                .iter()
                .map(|effect| effect.operation.as_str())
                .collect::<Vec<_>>(),
            ["tag:v1.2.3", "publish:archive", "publish:crate"]
        );
        assert_eq!(
            plan.lease_requirements.tree_mutating_phases,
            vec![PhaseInstanceId::new("stamp"), PhaseInstanceId::new("tag")]
        );
        assert!(plan
            .probes
            .iter()
            .any(|probe| probe.identity_channel == "tag_at_commit"));
        assert_eq!(
            plan.placement_instructions.terminal_state,
            "verified_staged_artifacts"
        );
        assert!(plan
            .placement_instructions
            .instruction
            .contains("does not place"));
        assert!(plan.phases.iter().all(|phase| phase.phase_type != "place"));
    }

    #[test]
    fn dry_run_refuses_incomplete_or_extra_artifact_material_without_provider_access() {
        let declaration = parse(DECLARATION).unwrap();
        let missing = build_dry_run_plan(
            RepositoryId::new("example-repository"),
            &declaration,
            "release",
            &finalized_artifacts()[..1],
        )
        .unwrap_err();
        assert!(matches!(
            missing,
            PlanError::MissingArtifact { ref artifact, .. } if artifact == "crate"
        ));

        let mut extra = finalized_artifacts();
        extra.push(FinalizedArtifact {
            artifact: ArtifactId::new("unexpected"),
            identity: "unexpected".to_owned(),
            bytes: Vec::new(),
        });
        let unexpected = build_dry_run_plan(
            RepositoryId::new("example-repository"),
            &declaration,
            "release",
            &extra,
        )
        .unwrap_err();
        assert!(matches!(
            unexpected,
            PlanError::UnexpectedArtifact { ref artifact, .. } if artifact == "unexpected"
        ));
    }
}
