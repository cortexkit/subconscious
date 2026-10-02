use cortexkit_release::declaration::{parse, DeclarationRefusalCode};

const SYNTHETIC_E2E_01: &str = include_str!("data/declarations/synthetic-e2e-01.release.jsonc");
const ALF_NOTAG_01: &str = include_str!("data/declarations/alf-notag-01.release.jsonc");
const AFT_CIW_01: &str = include_str!("data/declarations/aft-ciw-01-pre-tag-tests.release.jsonc");
const AFT_CIW_02: &str =
    include_str!("data/declarations/aft-ciw-02-post-tag-release.release.jsonc");
const FUTURE_VERSION: &str = include_str!("data/declarations/future-version.release.jsonc");

#[test]
fn future_declaration_version_refuses_before_planning_or_provider_access() {
    let error = parse(FUTURE_VERSION).expect_err("future format must fail closed");

    assert_eq!(error.code, DeclarationRefusalCode::UnsupportedFormatVersion);
    assert_eq!(error.location.unwrap().line, 3);
}

#[test]
fn normalization_makes_comments_key_order_and_whitespace_digest_insignificant() {
    let formatted = r#"
    {
      // presentation changes must not change a pinned declaration
      "trains": [],
      "version": 1,
    }
    "#;
    let compact = r#"{"version":1,"trains":[]}"#;

    let formatted_digest = parse(formatted).unwrap().digest;
    let compact_digest = parse(compact).unwrap().digest;

    assert_eq!(formatted_digest, compact_digest);
    assert_eq!(
        formatted_digest.as_str(),
        "bdb7da5b91024610c809316c37bd93cfef37fc25ab1c4260867d766299f19e0e"
    );
}

#[test]
fn adopter_case_synthetic_e2e_01_is_valid_and_identifies_its_first_trigger() {
    let parsed = parse(SYNTHETIC_E2E_01).unwrap();
    let train = &parsed.declaration.trains[0];

    assert_eq!(train.id, "synthetic");
    assert_eq!(train.phases[1].id, "publish-assets");
}

#[test]
fn adopter_case_alf_notag_01_uses_intended_commit_in_its_train_key() {
    let parsed = parse(ALF_NOTAG_01).unwrap();
    let train = &parsed.declaration.trains[0];

    assert_eq!(train.train_key(), "alf-deploy-a1b2c3d4");
    assert!(train.tag.is_none());
}

#[test]
fn adopter_cases_aft_ciw_01_and_02_keep_parameterized_watches_distinct() {
    let pre_tag = parse(AFT_CIW_01).unwrap();
    let post_tag = parse(AFT_CIW_02).unwrap();
    let pre_watch = &pre_tag.declaration.trains[0].phases[0];
    let post_watch = &post_tag.declaration.trains[0].phases[0];

    assert_ne!(pre_watch.id, post_watch.id);
    assert_ne!(pre_watch.params["workflow"], post_watch.params["workflow"]);
    assert_ne!(
        pre_watch.params["rerun_budget"],
        post_watch.params["rerun_budget"]
    );
}

#[test]
fn semantic_validation_rejects_late_refusal_with_both_phase_instance_ids() {
    let declaration = r#"
    {
      "version": 1,
      "trains": [{
        "id": "release",
        "intended_commit": "abc123",
        "tag": "v1.0.0",
        "signing_profile": "none",
        "operator_gates": ["first_public_trigger"],
        "artifacts": [{"id": "crate", "kind": "crate", "identity_channel": "registry_version"}],
        "phases": [
          {"id": "push-tag", "type": "tag"},
          {"id": "late-check", "type": "preflight"}
        ]
      }]
    }
    "#;

    let error = parse(declaration).expect_err("late refusal-capable phase must be rejected");
    assert_eq!(error.code, DeclarationRefusalCode::UnsafePhaseOrdering);
    assert!(error.message.contains("late-check"));
    assert!(error.message.contains("push-tag"));
}

#[test]
fn semantic_validation_refuses_duplicate_ids_unknown_types_and_unsafe_gates() {
    let duplicate = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"none","phases":[{"id":"same","type":"preflight"},{"id":"same","type":"build"}]}]}"#;
    assert_eq!(
        parse(duplicate).unwrap_err().code,
        DeclarationRefusalCode::DuplicatePhaseId
    );

    let unknown = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"none","phases":[{"id":"wat","type":"repo_code"}]}]}"#;
    assert_eq!(
        parse(unknown).unwrap_err().code,
        DeclarationRefusalCode::UnknownPhaseType
    );

    let missing_gate = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","tag":"v1","signing_profile":"none","artifacts":[{"id":"crate","kind":"crate","identity_channel":"registry_version"}],"phases":[{"id":"tag","type":"tag"}]}]}"#;
    assert_eq!(
        parse(missing_gate).unwrap_err().code,
        DeclarationRefusalCode::UnsafeOperatorGate
    );

    let unknown_gate = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"none","operator_gates":["publish_now"]}]}"#;
    assert_eq!(
        parse(unknown_gate).unwrap_err().code,
        DeclarationRefusalCode::UnsafeOperatorGate
    );
}

#[test]
fn semantic_validation_refuses_invalid_identity_signing_and_watch_parameters() {
    let missing_identity = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"none","artifacts":[{"id":"crate","kind":"crate"}]}]}"#;
    assert_eq!(
        parse(missing_identity).unwrap_err().code,
        DeclarationRefusalCode::MissingArtifactIdentityChannel
    );

    let invalid_signing =
        r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"made-up"}]}"#;
    assert_eq!(
        parse(invalid_signing).unwrap_err().code,
        DeclarationRefusalCode::InvalidSigningProfile
    );

    let invalid_watch = r#"{"version":1,"trains":[{"id":"t","intended_commit":"a","signing_profile":"none","phases":[{"id":"watch","type":"ci_watch","params":{"workflow":"Tests"}}]}]}"#;
    assert_eq!(
        parse(invalid_watch).unwrap_err().code,
        DeclarationRefusalCode::InvalidPhaseParameters
    );
}

#[test]
fn precheck_parameter_schemas_reject_unknown_fields() {
    let declaration = r#"{
      "version": 1,
      "trains": [{
        "id": "strict-precheck",
        "intended_commit": "abc123",
        "signing_profile": "none",
        "phases": [{
          "id": "dirty",
          "type": "precheck-format-dirty",
          "params": {
            "tool": "cargo fmt",
            "command": ["cargo", "fmt", "--check"],
            "invented": true
          }
        }]
      }]
    }"#;

    let error = parse(declaration).expect_err("precheck params are a closed schema");
    assert_eq!(error.code, DeclarationRefusalCode::InvalidPhaseParameters);
    assert!(error.message.contains("unknown field `invented`"));
}

#[test]
fn overlapping_publication_targets_refuse_with_artifact_and_phase_names() {
    let declaration = r#"{"version":1,"trains":[{"id":"release","intended_commit":"a","tag":"v1","signing_profile":"none","operator_gates":["first_public_trigger"],"artifacts":[{"id":"archive","kind":"archive","identity_channel":"asset_sha256"}],"phases":[{"id":"registry-publish","type":"publish"},{"id":"release-assets","type":"assets"}]}]}"#;
    let error =
        parse(declaration).expect_err("overlapping publication must refuse before execution");
    assert_eq!(error.code, DeclarationRefusalCode::InvalidPhaseParameters);
    for name in ["archive", "registry-publish", "release-assets"] {
        assert!(error.message.contains(name), "{}", error.message);
    }
}

#[test]
fn partitioned_publication_targets_plan_each_artifact_once() {
    use cortexkit_release::plan::{build_dry_run_plan, FinalizedArtifact};
    let source = r#"{"version":1,"trains":[{"id":"release","intended_commit":"a","tag":"v1","signing_profile":"none","operator_gates":["first_public_trigger"],"artifacts":[{"id":"crate","kind":"crate","identity_channel":"registry_version"},{"id":"archive","kind":"archive","identity_channel":"asset_sha256"}],"phases":[{"id":"registry-publish","type":"publish","params":{"artifacts":["crate"]}},{"id":"release-assets","type":"assets","params":{"artifacts":["archive"]}}]}]}"#;
    let declaration = parse(source).unwrap();
    let material = ["crate", "archive"].map(|id| FinalizedArtifact {
        artifact: id.into(),
        identity: "a".to_owned(),
        bytes: vec![1],
    });
    let plan = build_dry_run_plan("scratch".into(), &declaration, "release", &material).unwrap();
    let targets = plan
        .public_effects
        .iter()
        .map(|effect| {
            (
                effect.phase.as_str(),
                effect.artifact.as_ref().unwrap().as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        [("registry-publish", "crate"), ("release-assets", "archive")]
    );
    assert_eq!(plan.probes.len(), 2);
    for (effect, probe) in plan.public_effects.iter().zip(&plan.probes) {
        assert_eq!(effect.phase, probe.phase);
        assert_eq!(effect.artifact, probe.artifact);
    }
}

#[test]
fn publication_targets_must_be_nonempty_unique_declared_artifact_ids() {
    for targets in [
        "[]",
        "[\"unknown\"]",
        "[\"archive\",\"archive\"]",
        "[7]",
        "\"archive\"",
    ] {
        let source = format!(
            r#"{{"version":1,"trains":[{{"id":"release","intended_commit":"a","tag":"v1","signing_profile":"none","operator_gates":["first_public_trigger"],"artifacts":[{{"id":"archive","kind":"archive","identity_channel":"asset_sha256"}}],"phases":[{{"id":"publish","type":"publish","params":{{"artifacts":{targets}}}}}]}}]}}"#
        );
        assert_eq!(
            parse(&source).unwrap_err().code,
            DeclarationRefusalCode::InvalidPhaseParameters,
            "{targets}"
        );
    }
}
