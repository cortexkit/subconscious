//! Versioned `.cortexkit/release.jsonc` declarations.
//!
//! The declaration is parsed before any caller can construct a plan or obtain a
//! provider. Its digest is derived from canonical JSON rather than the source
//! text, so comments and presentation-only changes cannot alter a train's
//! pinned declaration identity.

use crate::{CommitId, DeclarationDigest, PhaseInstanceId, TrainId};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, path::Path};
use thiserror::Error;

const SUPPORTED_FORMAT_VERSION: u32 = 1;
const PHASE_TYPES: &[&str] = &[
    "preflight",
    "gates_local",
    "ci_watch",
    "build",
    "stamp",
    "verify_readback",
    "tag",
    "publish",
    "assets",
    "stage",
    "notify",
    crate::phases::precheck::FORMAT_DIRTY,
    crate::phases::precheck::STALE_RESIDUE,
    crate::phases::precheck::SIBLING_DRIFT,
    crate::phases::precheck::CONTEXT_FITNESS,
    crate::phases::precheck::TOOL_PINNING,
    crate::phases::precheck::RESIDUE_SWEEP,
];
const IDENTITY_CHANNELS: &[&str] = &[
    "tag_at_commit",
    "registry_version",
    "asset_sha256",
    "gh_release",
    "embedded_build_sha",
];
const SIGNING_PROFILES: &[&str] = &["none", "minisign", "cosign", "apple_codesign"];
const OPERATOR_GATES: &[&str] = &["first_public_trigger"];

/// The location of a declaration value in its original JSONC source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    pub line: usize,
    pub column: usize,
}

/// Stable machine-readable reasons for refusing a declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclarationRefusalCode {
    Parse,
    UnsupportedFormatVersion,
    DuplicateTrainId,
    DuplicatePhaseId,
    DuplicateArtifactId,
    UnknownPhaseType,
    InvalidPhaseParameters,
    MissingArtifactIdentityChannel,
    InvalidArtifactIdentityChannel,
    InvalidSigningProfile,
    UnsafeOperatorGate,
    MissingFirstPublicTrigger,
    UnsafePhaseOrdering,
    InvalidNoTagTrain,
}

/// A typed declaration refusal with the source location when it is known.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{code:?}: {message}")]
pub struct DeclarationError {
    pub code: DeclarationRefusalCode,
    pub message: String,
    pub location: Option<SourceLocation>,
}

impl DeclarationError {
    fn new(
        code: DeclarationRefusalCode,
        message: impl Into<String>,
        location: Option<SourceLocation>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            location,
        }
    }
}

/// A parsed declaration and its source-independent identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDeclaration {
    pub declaration: ReleaseDeclaration,
    /// The canonical JSON value whose digest identifies this declaration.
    pub normalized: Value,
    pub digest: DeclarationDigest,
}

/// The format-v1 release declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDeclaration {
    pub version: u32,
    pub trains: Vec<TrainDeclaration>,
}

/// One independently named release train.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrainDeclaration {
    pub id: String,
    pub intended_commit: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub phases: Vec<PhaseDeclaration>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDeclaration>,
    pub signing_profile: String,
    #[serde(default)]
    pub operator_gates: Vec<String>,
}

impl TrainDeclaration {
    /// Returns the durable key used by no-tag trains.
    pub fn train_key(&self) -> String {
        match &self.tag {
            Some(tag) => format!("{}-{tag}", self.id),
            None => format!("{}-{}", self.id, self.intended_commit),
        }
    }

    pub fn train_id(&self) -> TrainId {
        TrainId::new(&self.id)
    }

    pub fn intended_commit_id(&self) -> CommitId {
        CommitId::new(&self.intended_commit)
    }
}

/// One named, parameterized instance from the machine-owned phase registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PhaseDeclaration {
    pub id: String,
    #[serde(rename = "type")]
    pub phase_type: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl PhaseDeclaration {
    pub fn instance_id(&self) -> PhaseInstanceId {
        PhaseInstanceId::new(&self.id)
    }
}

/// One artifact together with the evidence used by its completion probe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDeclaration {
    pub id: String,
    pub kind: String,
    pub identity_channel: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

fn sort_object_keys(value: &mut Value) {
    // serde_json maps can preserve insertion order when another workspace package
    // enables that feature, so rebuild every object in lexical order before hashing.
    match value {
        Value::Array(values) => values.iter_mut().for_each(sort_object_keys),
        Value::Object(object) => {
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            for (_, value) in &mut entries {
                sort_object_keys(value);
            }
            object.extend(entries);
        }
        _ => {}
    }
}

/// Parses and validates a JSONC declaration from disk.
pub fn load(path: impl AsRef<Path>) -> Result<ParsedDeclaration, DeclarationError> {
    let path = path.as_ref();
    let source = fs::read_to_string(path).map_err(|error| {
        DeclarationError::new(
            DeclarationRefusalCode::Parse,
            format!("cannot read {}: {error}", path.display()),
            None,
        )
    })?;
    parse(&source)
}

/// Parses and validates a JSONC declaration without accessing providers.
pub fn parse(source: &str) -> Result<ParsedDeclaration, DeclarationError> {
    let json = subc_jsonc::jsonc_to_json(source).map_err(|message| {
        DeclarationError::new(
            DeclarationRefusalCode::Parse,
            message,
            source_location(source, "version"),
        )
    })?;
    let mut value: Value = serde_json::from_str(&json).map_err(|error| {
        DeclarationError::new(
            DeclarationRefusalCode::Parse,
            error.to_string(),
            Some(SourceLocation {
                line: error.line(),
                column: error.column(),
            }),
        )
    })?;
    sort_object_keys(&mut value);

    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            DeclarationError::new(
                DeclarationRefusalCode::UnsupportedFormatVersion,
                "release declaration must contain an integer version",
                source_location(source, "version"),
            )
        })?;
    if version != u64::from(SUPPORTED_FORMAT_VERSION) {
        return Err(DeclarationError::new(
            DeclarationRefusalCode::UnsupportedFormatVersion,
            format!(
                "release declaration version {version} is unsupported; supported version is {SUPPORTED_FORMAT_VERSION}"
            ),
            source_location(source, "version"),
        ));
    }

    let declaration: ReleaseDeclaration =
        serde_json::from_value(value.clone()).map_err(|error| {
            DeclarationError::new(
                DeclarationRefusalCode::Parse,
                error.to_string(),
                source_location(source, field_from_serde_error(&error.to_string())),
            )
        })?;
    validate(&declaration, source)?;

    let canonical = serde_json::to_vec(&value).expect("serde_json::Value always serializes");
    let digest = DeclarationDigest::new(format!("{:x}", Sha256::digest(canonical)));
    Ok(ParsedDeclaration {
        declaration,
        normalized: value,
        digest,
    })
}

/// Validates a parsed declaration before it can be planned or executed.
pub fn validate(declaration: &ReleaseDeclaration, source: &str) -> Result<(), DeclarationError> {
    if declaration.version != SUPPORTED_FORMAT_VERSION {
        return Err(DeclarationError::new(
            DeclarationRefusalCode::UnsupportedFormatVersion,
            format!(
                "release declaration version {} is unsupported",
                declaration.version
            ),
            source_location(source, "version"),
        ));
    }

    let mut train_ids = HashSet::new();
    for train in &declaration.trains {
        if train.id.trim().is_empty() || !train_ids.insert(&train.id) {
            return Err(refusal(
                DeclarationRefusalCode::DuplicateTrainId,
                format!("duplicate or empty train id `{}`", train.id),
                source,
                "id",
            ));
        }
        validate_train(train, source)?;
    }
    Ok(())
}

fn validate_train(train: &TrainDeclaration, source: &str) -> Result<(), DeclarationError> {
    if train.intended_commit.trim().is_empty() {
        return Err(refusal(
            DeclarationRefusalCode::InvalidPhaseParameters,
            format!("train `{}` has no intended commit", train.id),
            source,
            "intended_commit",
        ));
    }
    if !SIGNING_PROFILES.contains(&train.signing_profile.as_str()) {
        return Err(refusal(
            DeclarationRefusalCode::InvalidSigningProfile,
            format!(
                "train `{}` selects unsupported signing profile `{}`",
                train.id, train.signing_profile
            ),
            source,
            "signing_profile",
        ));
    }

    let mut gates = HashSet::new();
    for gate in &train.operator_gates {
        if !OPERATOR_GATES.contains(&gate.as_str()) || !gates.insert(gate) {
            return Err(refusal(
                DeclarationRefusalCode::UnsafeOperatorGate,
                format!(
                    "train `{}` selects invalid or duplicate operator gate `{gate}`",
                    train.id
                ),
                source,
                "operator_gates",
            ));
        }
    }

    let mut artifact_ids = HashSet::new();
    for artifact in &train.artifacts {
        if artifact.id.trim().is_empty() || !artifact_ids.insert(&artifact.id) {
            return Err(refusal(
                DeclarationRefusalCode::DuplicateArtifactId,
                format!(
                    "train `{}` has duplicate or empty artifact id `{}`",
                    train.id, artifact.id
                ),
                source,
                "id",
            ));
        }
        let Some(channel) = artifact.identity_channel.as_deref() else {
            return Err(refusal(
                DeclarationRefusalCode::MissingArtifactIdentityChannel,
                format!("artifact `{}` has no identity channel", artifact.id),
                source,
                "identity_channel",
            ));
        };
        if !IDENTITY_CHANNELS.contains(&channel) {
            return Err(refusal(
                DeclarationRefusalCode::InvalidArtifactIdentityChannel,
                format!(
                    "artifact `{}` selects unknown identity channel `{channel}`",
                    artifact.id
                ),
                source,
                "identity_channel",
            ));
        }
        if artifact.kind.trim().is_empty() {
            return Err(refusal(
                DeclarationRefusalCode::InvalidPhaseParameters,
                format!("artifact `{}` has no kind", artifact.id),
                source,
                "kind",
            ));
        }
    }

    let mut phase_ids = HashSet::new();
    let mut first_irreversible: Option<&PhaseDeclaration> = None;
    for phase in &train.phases {
        if phase.id.trim().is_empty() || !phase_ids.insert(&phase.id) {
            return Err(refusal(
                DeclarationRefusalCode::DuplicatePhaseId,
                format!(
                    "train `{}` has duplicate or empty phase id `{}`",
                    train.id, phase.id
                ),
                source,
                "id",
            ));
        }
        if !PHASE_TYPES.contains(&phase.phase_type.as_str()) {
            return Err(refusal(
                DeclarationRefusalCode::UnknownPhaseType,
                format!(
                    "phase `{}` uses unknown type `{}`",
                    phase.id, phase.phase_type
                ),
                source,
                "type",
            ));
        }
        validate_phase_parameters(phase, train, source)?;
        if let Some(earlier) = first_irreversible {
            if refusal_capable(&phase.phase_type) {
                return Err(refusal(
                    DeclarationRefusalCode::UnsafePhaseOrdering,
                    format!(
                        "refusal-capable phase `{}` first executes after irreversible phase `{}`",
                        phase.id, earlier.id
                    ),
                    source,
                    "id",
                ));
            }
        } else if irreversible_public(&phase.phase_type) {
            first_irreversible = Some(phase);
        }
    }

    if let Some(trigger) = first_irreversible {
        if !train
            .operator_gates
            .iter()
            .any(|gate| gate == "first_public_trigger")
        {
            return Err(refusal(
                DeclarationRefusalCode::UnsafeOperatorGate,
                format!(
                    "train `{}` must declare the first_public_trigger gate before `{}`",
                    train.id, trigger.id
                ),
                source,
                "operator_gates",
            ));
        }
        if !first_trigger_identifiable(trigger, train) {
            return Err(refusal(
                DeclarationRefusalCode::MissingFirstPublicTrigger,
                format!(
                    "first public trigger `{}` lacks a tag or declared artifact identity",
                    trigger.id
                ),
                source,
                "params",
            ));
        }
    } else if !train.operator_gates.is_empty() {
        return Err(refusal(
            DeclarationRefusalCode::UnsafeOperatorGate,
            format!(
                "train `{}` declares a public-trigger gate without a public trigger",
                train.id
            ),
            source,
            "operator_gates",
        ));
    }

    if train.tag.is_none()
        && train
            .phases
            .iter()
            .any(|phase| matches!(phase.phase_type.as_str(), "tag" | "publish"))
    {
        return Err(refusal(
            DeclarationRefusalCode::InvalidNoTagTrain,
            format!(
                "no-tag train `{}` cannot declare tag or publish phases",
                train.id
            ),
            source,
            "type",
        ));
    }
    Ok(())
}

fn validate_phase_parameters(
    phase: &PhaseDeclaration,
    train: &TrainDeclaration,
    source: &str,
) -> Result<(), DeclarationError> {
    let params = phase.params.as_object().ok_or_else(|| {
        refusal(
            DeclarationRefusalCode::InvalidPhaseParameters,
            format!("phase `{}` parameters must be an object", phase.id),
            source,
            "params",
        )
    })?;
    if let Err(message) = crate::phases::precheck::validate_parameters(phase) {
        return Err(refusal(
            DeclarationRefusalCode::InvalidPhaseParameters,
            format!("phase `{}` has invalid parameters: {message}", phase.id),
            source,
            "params",
        ));
    }
    if phase.phase_type == "ci_watch" {
        let workflow = params.get("workflow").and_then(Value::as_str);
        let selector = params.get("selector").and_then(Value::as_str);
        let budget = params.get("rerun_budget").and_then(Value::as_u64);
        if !matches!(workflow, Some(value) if !value.trim().is_empty())
            || !matches!(selector, Some(value) if !value.trim().is_empty())
            || budget.is_none()
        {
            return Err(refusal(
                DeclarationRefusalCode::InvalidPhaseParameters,
                format!(
                    "ci_watch phase `{}` requires non-empty workflow and selector plus rerun_budget",
                    phase.id
                ),
                source,
                "params",
            ));
        }
    }
    if phase.phase_type == "tag"
        && train.tag.as_deref().is_none_or(str::is_empty)
        && !matches!(params.get("tag").and_then(Value::as_str), Some(value) if !value.is_empty())
    {
        return Err(refusal(
            DeclarationRefusalCode::MissingFirstPublicTrigger,
            format!("tag phase `{}` has no tag identity", phase.id),
            source,
            "tag",
        ));
    }
    Ok(())
}

fn first_trigger_identifiable(trigger: &PhaseDeclaration, train: &TrainDeclaration) -> bool {
    match trigger.phase_type.as_str() {
        "tag" => {
            train.tag.as_deref().is_some_and(|tag| !tag.is_empty())
                || trigger
                    .params
                    .get("tag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| !tag.is_empty())
        }
        "publish" | "assets" => !train.artifacts.is_empty(),
        _ => false,
    }
}

fn irreversible_public(phase_type: &str) -> bool {
    matches!(phase_type, "tag" | "publish" | "assets")
}

fn refusal_capable(phase_type: &str) -> bool {
    !matches!(
        phase_type,
        "tag" | "publish" | "assets" | "stage" | "notify"
    )
}

fn refusal(
    code: DeclarationRefusalCode,
    message: String,
    source: &str,
    field: &str,
) -> DeclarationError {
    DeclarationError::new(code, message, source_location(source, field))
}

fn field_from_serde_error(error: &str) -> &str {
    error
        .split('`')
        .nth(1)
        .filter(|field| !field.is_empty())
        .unwrap_or("version")
}

fn source_location(source: &str, key: &str) -> Option<SourceLocation> {
    let needle = format!("\"{key}\"");
    let offset = source.find(&needle)?;
    let prefix = &source[..offset];
    Some(SourceLocation {
        line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
        column: prefix
            .rsplit_once('\n')
            .map_or(prefix.len() + 1, |(_, line)| line.len() + 1),
    })
}
