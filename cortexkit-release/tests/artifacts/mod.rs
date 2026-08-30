use cortexkit_release::{
    approval::{ApprovalSubject, ApprovedArtifact},
    artifact::{
        artifact_digest, finalize_artifact, verify_provider_evidence, AnchoredSelectionAuthority,
        ArtifactError, ArtifactIdentity, ArtifactInput, ArtifactPath, ArtifactReleaseIdentity,
        ArtifactSelectionKind, ArtifactSelections, ProviderArtifactEvidence, RecordedArtifactCall,
        RecordingArtifactTransformer,
    },
    executor::{
        publish_finalized_artifact, stage_finalized_artifact, ExecutorError, PublicationRequest,
        RecordedExecutorCall, RecordingArtifactExecutor, StageRequest, StagingEvidence,
    },
    plan::{PublicEffect, ReleaseIdentity},
    ArtifactId, CommitId, DeclarationDigest, EffectRequest, OperationId, PhaseInstanceId,
    RepositoryId, TrainId, NORMATIVE_SPEC_REFERENCE,
};

struct AnchorSelections;

impl AnchoredSelectionAuthority for AnchorSelections {
    fn supports(
        &self,
        normative_reference: &str,
        _artifact_kind: &str,
        _selection_kind: ArtifactSelectionKind,
        value: &str,
    ) -> bool {
        normative_reference == NORMATIVE_SPEC_REFERENCE && !value.is_empty()
    }
}

struct RefusingIdentityChannel;

impl AnchoredSelectionAuthority for RefusingIdentityChannel {
    fn supports(
        &self,
        _normative_reference: &str,
        _artifact_kind: &str,
        selection_kind: ArtifactSelectionKind,
        _value: &str,
    ) -> bool {
        selection_kind != ArtifactSelectionKind::IdentityChannel
    }
}

fn selections() -> ArtifactSelections {
    ArtifactSelections {
        identity_channel: "selected-by-anchor".to_owned(),
        signing_profile: "selected-by-anchor".to_owned(),
        staging_backend: "selected-by-anchor".to_owned(),
        load_class: "selected-by-anchor".to_owned(),
    }
}

fn tagged_identity() -> ArtifactIdentity {
    ArtifactIdentity::new(
        ArtifactId::new("release-archive"),
        "archive",
        ArtifactReleaseIdentity::Tagged {
            tag: "v1.2.3".to_owned(),
        },
        CommitId::new("commit-a"),
        selections(),
        &AnchorSelections,
    )
    .unwrap()
}

fn no_tag_identity() -> ArtifactIdentity {
    ArtifactIdentity::new(
        ArtifactId::new("deployed-binary"),
        "binary",
        ArtifactReleaseIdentity::NoTag,
        CommitId::new("commit-b"),
        selections(),
        &AnchorSelections,
    )
    .unwrap()
}

fn effect(artifact: ArtifactId, commit: &str) -> EffectRequest {
    EffectRequest {
        repository: RepositoryId::new("example/repository"),
        train: TrainId::new("release"),
        phase: PhaseInstanceId::new("publish"),
        artifact,
        operation: OperationId::new("publish:artifact"),
        intended_commit: CommitId::new(commit),
        declaration_digest: DeclarationDigest::new("declaration-digest"),
    }
}

fn approval(
    effect: &EffectRequest,
    artifact: &cortexkit_release::artifact::FinalizedArtifact,
    release: ReleaseIdentity,
) -> ApprovalSubject {
    ApprovalSubject {
        repository: effect.repository.clone(),
        train: effect.train.clone(),
        intended_commit: effect.intended_commit.clone(),
        declaration_digest: effect.declaration_digest.clone(),
        artifacts: vec![ApprovedArtifact {
            artifact: effect.artifact.clone(),
            identity: artifact.identity().approval_identity(),
            digest: artifact.digest().to_owned(),
        }],
        version_or_run_id: release,
        public_effects: vec![PublicEffect {
            phase: effect.phase.clone(),
            operation: effect.operation.clone(),
            artifact: Some(effect.artifact.clone()),
        }],
    }
}

#[test]
fn raw_artifact_is_verified_then_finalized_and_published_with_its_exact_approval_digest() {
    let input = ArtifactInput {
        identity: tagged_identity(),
        path: ArtifactPath::Raw,
        bytes: b"raw bytes".to_vec(),
    };
    let mut transformer = RecordingArtifactTransformer::new(
        b"stripped bytes".to_vec(),
        b"signed bytes".to_vec(),
        b"sidecar from signed bytes".to_vec(),
    );

    let artifact = finalize_artifact(&mut transformer, input.clone()).unwrap();
    assert_eq!(artifact.digest(), artifact_digest(b"signed bytes"));
    assert_eq!(
        transformer.calls(),
        [
            RecordedArtifactCall::VerifyRaw(input.clone()),
            RecordedArtifactCall::Strip {
                artifact: input.identity.artifact().clone(),
                bytes: b"raw bytes".to_vec(),
            },
            RecordedArtifactCall::Sign {
                artifact: input.identity.artifact().clone(),
                bytes: b"stripped bytes".to_vec(),
            },
            RecordedArtifactCall::SidecarFromSignedBytes {
                artifact: input.identity.artifact().clone(),
                bytes: b"signed bytes".to_vec(),
            },
            RecordedArtifactCall::VerifyReadback {
                artifact: input.identity.artifact().clone(),
                signed_bytes: b"signed bytes".to_vec(),
                sidecar: b"sidecar from signed bytes".to_vec(),
            },
        ]
    );

    let effect = effect(artifact.identity().artifact().clone(), "commit-a");
    let request = PublicationRequest::new(
        effect.clone(),
        artifact.clone(),
        approval(
            &effect,
            &artifact,
            ReleaseIdentity::Version("v1.2.3".to_owned()),
        ),
    )
    .unwrap();
    let published_evidence = ProviderArtifactEvidence {
        reference: "registry/release-archive".to_owned(),
        artifact: artifact.identity().artifact().clone(),
        commit: CommitId::new("commit-a"),
        release: Some("v1.2.3".to_owned()),
        embedded_build_sha: None,
    };
    let staging_evidence = StagingEvidence {
        reference: "stage/release-archive".to_owned(),
        artifact: artifact.identity().artifact().clone(),
        digest: artifact.digest().to_owned(),
    };
    let mut executor = RecordingArtifactExecutor::new(
        [Ok(published_evidence.clone())],
        [Ok(staging_evidence.clone())],
    );

    assert_eq!(
        publish_finalized_artifact(&mut executor, &request).unwrap(),
        published_evidence
    );
    let stage_request = StageRequest::new(artifact).unwrap();
    assert_eq!(
        stage_finalized_artifact(&mut executor, &stage_request).unwrap(),
        staging_evidence
    );
    assert_eq!(
        executor.calls(),
        [
            RecordedExecutorCall::Publish(Box::new(request)),
            RecordedExecutorCall::Stage(Box::new(stage_request)),
        ]
    );
}

#[test]
fn transformed_no_tag_artifact_requires_matching_compiler_emitted_build_sha() {
    let input = ArtifactInput {
        identity: no_tag_identity(),
        path: ArtifactPath::Transformed,
        bytes: b"build output".to_vec(),
    };
    let mut transformer = RecordingArtifactTransformer::new(
        b"stripped".to_vec(),
        b"signed".to_vec(),
        b"signed-sidecar".to_vec(),
    );
    let artifact = finalize_artifact(&mut transformer, input.clone()).unwrap();

    assert_eq!(
        transformer.calls(),
        [
            RecordedArtifactCall::Strip {
                artifact: input.identity.artifact().clone(),
                bytes: b"build output".to_vec(),
            },
            RecordedArtifactCall::Sign {
                artifact: input.identity.artifact().clone(),
                bytes: b"stripped".to_vec(),
            },
            RecordedArtifactCall::SidecarFromSignedBytes {
                artifact: input.identity.artifact().clone(),
                bytes: b"signed".to_vec(),
            },
            RecordedArtifactCall::VerifyReadback {
                artifact: input.identity.artifact().clone(),
                signed_bytes: b"signed".to_vec(),
                sidecar: b"signed-sidecar".to_vec(),
            },
        ]
    );

    let effect = effect(artifact.identity().artifact().clone(), "commit-b");
    let request = PublicationRequest::new(
        effect.clone(),
        artifact.clone(),
        approval(
            &effect,
            &artifact,
            ReleaseIdentity::RunId("release-commit-b".to_owned()),
        ),
    )
    .unwrap();
    let evidence = ProviderArtifactEvidence {
        reference: "staged/deployed-binary".to_owned(),
        artifact: artifact.identity().artifact().clone(),
        commit: CommitId::new("commit-b"),
        release: None,
        embedded_build_sha: Some(CommitId::new("commit-b")),
    };
    let mut executor = RecordingArtifactExecutor::new(
        [Ok(evidence.clone())],
        std::iter::empty::<Result<StagingEvidence, ExecutorError>>(),
    );

    assert_eq!(
        publish_finalized_artifact(&mut executor, &request).unwrap(),
        evidence
    );
    assert_eq!(
        executor.calls(),
        [RecordedExecutorCall::Publish(Box::new(request))]
    );
}

#[test]
fn unsupported_identity_channels_and_contradictory_provider_evidence_refuse() {
    let missing_channel = ArtifactIdentity::new(
        ArtifactId::new("unidentified"),
        "binary",
        ArtifactReleaseIdentity::NoTag,
        CommitId::new("commit-c"),
        ArtifactSelections {
            identity_channel: String::new(),
            ..selections()
        },
        &AnchorSelections,
    )
    .unwrap_err();
    assert!(matches!(
        missing_channel,
        ArtifactError::MissingIdentityChannel { .. }
    ));

    let unsupported_channel = ArtifactIdentity::new(
        ArtifactId::new("unsupported"),
        "binary",
        ArtifactReleaseIdentity::NoTag,
        CommitId::new("commit-c"),
        selections(),
        &RefusingIdentityChannel,
    )
    .unwrap_err();
    assert!(matches!(
        unsupported_channel,
        ArtifactError::UnsupportedSelection {
            selection: "identity channel",
            ..
        }
    ));

    let identity = no_tag_identity();
    for evidence in [
        ProviderArtifactEvidence {
            reference: "provider/wrong-artifact".to_owned(),
            artifact: ArtifactId::new("different-artifact"),
            commit: CommitId::new("commit-b"),
            release: None,
            embedded_build_sha: Some(CommitId::new("commit-b")),
        },
        ProviderArtifactEvidence {
            reference: "provider/wrong-commit".to_owned(),
            artifact: identity.artifact().clone(),
            commit: CommitId::new("different-commit"),
            release: None,
            embedded_build_sha: Some(CommitId::new("commit-b")),
        },
        ProviderArtifactEvidence {
            reference: "provider/wrong-release".to_owned(),
            artifact: identity.artifact().clone(),
            commit: CommitId::new("commit-b"),
            release: Some("v9.9.9".to_owned()),
            embedded_build_sha: Some(CommitId::new("commit-b")),
        },
        ProviderArtifactEvidence {
            reference: "provider/wrong-build".to_owned(),
            artifact: identity.artifact().clone(),
            commit: CommitId::new("commit-b"),
            release: None,
            embedded_build_sha: Some(CommitId::new("different-build")),
        },
    ] {
        assert!(matches!(
            verify_provider_evidence(&identity, &evidence),
            Err(ArtifactError::ProviderEvidenceMismatch { .. })
        ));
    }

    let missing_build_sha = ProviderArtifactEvidence {
        reference: "provider/missing-build".to_owned(),
        artifact: identity.artifact().clone(),
        commit: CommitId::new("commit-b"),
        release: None,
        embedded_build_sha: None,
    };
    assert!(matches!(
        verify_provider_evidence(&identity, &missing_build_sha),
        Err(ArtifactError::MissingEmbeddedBuildSha { .. })
    ));

    let invalid_approval = ApprovalSubject {
        repository: RepositoryId::new("example/repository"),
        train: TrainId::new("release"),
        intended_commit: CommitId::new("commit-b"),
        declaration_digest: DeclarationDigest::new("declaration-digest"),
        artifacts: Vec::new(),
        version_or_run_id: ReleaseIdentity::RunId("release-commit-b".to_owned()),
        public_effects: Vec::new(),
    };
    let input = ArtifactInput {
        identity,
        path: ArtifactPath::Transformed,
        bytes: b"build output".to_vec(),
    };
    let mut transformer = RecordingArtifactTransformer::new(b"stripped", b"signed", b"sidecar");
    let artifact = finalize_artifact(&mut transformer, input).unwrap();
    assert!(matches!(
        PublicationRequest::new(
            effect(artifact.identity().artifact().clone(), "commit-b"),
            artifact,
            invalid_approval
        ),
        Err(ExecutorError::ApprovalBinding { .. })
    ));
}
