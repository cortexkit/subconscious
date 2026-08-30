#![forbid(unsafe_code)]

//! Shared, provider-neutral primitives for the `ck-release` machine.
//!
//! The machine's supported phases, provider operations, artifact identity
//! channels, and signing profiles are incorporated only from
//! `docs/specs/fleet-release-machine.md@41cb2be4`. This crate deliberately
//! carries opaque identifiers at that boundary instead of duplicating those
//! closed sets in Rust enums.

pub mod approval;
pub mod declaration;
pub mod lease;
pub mod plan;

use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

pub mod state;

/// The immutable release-machine specification incorporated by this package.
pub const NORMATIVE_SPEC_REFERENCE: &str = "docs/specs/fleet-release-machine.md@41cb2be4";

macro_rules! opaque_identifier {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(
            Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates an identifier without assigning semantics to its contents.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the identifier exactly as supplied by the caller.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the identifier and returns its unmodified value.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

opaque_identifier!(
    RepositoryId,
    "A stable identifier for one declared repository."
);
opaque_identifier!(TrainId, "A declared release-train identifier.");
opaque_identifier!(
    PhaseInstanceId,
    "An identifier for one parameterized phase instance."
);
opaque_identifier!(ArtifactId, "An identifier for one artifact within a train.");
opaque_identifier!(OperationId, "An opaque provider-operation identifier.");
opaque_identifier!(
    CommitId,
    "The intended source-commit identifier for a train."
);
opaque_identifier!(
    DeclarationDigest,
    "The normalized release-declaration digest."
);
opaque_identifier!(ApprovalToken, "A durable operator-approval token.");

/// A request for one irreversible per-artifact operation.
///
/// `operation` is deliberately opaque: the anchored specification owns the
/// closed provider-operation vocabulary, while this primitive preserves the
/// identity needed for journaling, approval, and replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectRequest {
    pub repository: RepositoryId,
    pub train: TrainId,
    pub phase: PhaseInstanceId,
    pub artifact: ArtifactId,
    pub operation: OperationId,
    pub intended_commit: CommitId,
    pub declaration_digest: DeclarationDigest,
}

/// Evidence returned by a completion probe or irreversible executor.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeEvidence {
    /// The provider's durable reference for the observed effect.
    pub reference: String,
    /// The observed identity used to compare the effect with the request.
    pub identity: String,
}

/// A probe result that preserves the distinction between absence and delay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProbeResult {
    /// The intended effect exists and has matching evidence.
    Present(ProbeEvidence),
    /// The intended effect is authoritatively absent.
    Absent(ProbeEvidence),
    /// The provider cannot decide before its declared settle deadline.
    Undecidable(UndecidableProbe),
}

/// Retry guidance carried by an undecidable completion probe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UndecidableProbe {
    pub reason: String,
    pub retry_after_ms: u64,
    pub settle_deadline_ms: u64,
}

/// The complete subject that must be approved before the first public trigger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalSubject {
    pub repository: RepositoryId,
    pub train: TrainId,
    pub intended_commit: CommitId,
    pub declaration_digest: DeclarationDigest,
    pub artifact_digests: Vec<ArtifactDigest>,
    pub public_effects: Vec<OperationId>,
}

/// A finalized artifact digest included in an approval subject.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactDigest {
    pub artifact: ArtifactId,
    pub digest: String,
}

/// One append request for durable machine state.
///
/// The journal module owns record encoding and checksums. Keeping this seam at
/// bytes lets tests observe exactly what durable effect was requested without
/// defining a second record vocabulary here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableWrite {
    pub stream: String,
    pub bytes: Vec<u8>,
}

/// A seam failure that carries no provider-specific behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeamError {
    message: String,
}

impl SeamError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SeamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SeamError {}

/// Observes whether an intended external effect already exists.
pub trait CompletionProbe {
    fn probe(&mut self, request: &EffectRequest) -> Result<ProbeResult, SeamError>;
}

/// Performs one irreversible external effect after admission by the machine.
pub trait IrreversibleExecutor {
    fn execute(&mut self, request: &EffectRequest) -> Result<ProbeEvidence, SeamError>;
}

/// Persists an already-encoded journal or intent write.
pub trait DurableState {
    fn append(&mut self, write: &DurableWrite) -> Result<(), SeamError>;
}

/// Requests explicit approval for one fully bound public-effect list.
pub trait ApprovalGate {
    fn approve(&mut self, subject: &ApprovalSubject) -> Result<ApprovalToken, SeamError>;
}

// The package publishes the `ck-release` binary before command handling is
// introduced. Keeping this placeholder side-effect-free means early package
// and lockfile verification cannot accidentally invoke a release operation.
#[allow(dead_code)]
fn main() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_identifiers_preserve_values_and_serde_shape() {
        let train = TrainId::new("release-main");

        assert_eq!(train.as_str(), "release-main");
        assert_eq!(serde_json::to_string(&train).unwrap(), "\"release-main\"");
        assert_eq!(
            serde_json::from_str::<TrainId>("\"release-main\"").unwrap(),
            train
        );
    }

    #[test]
    fn probe_result_keeps_undecidable_distinct_from_absent() {
        let result = ProbeResult::Undecidable(UndecidableProbe {
            reason: "registry index is still settling".to_owned(),
            retry_after_ms: 250,
            settle_deadline_ms: 5_000,
        });

        assert!(matches!(result, ProbeResult::Undecidable(_)));
        assert!(!matches!(result, ProbeResult::Absent(_)));
    }
}
