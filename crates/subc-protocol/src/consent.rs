//! Shared consent/v1 plan card bodies for the daemon requester and responders.
//!
//! A reply can approve only the pending request on the daemon-owned connection,
//! after that connection's trust and active registration are rechecked. Matching
//! these body fields is necessary, never sufficient authority. Rendering a card
//! or asserting that a terminal showed a diff is not approval.

use serde::{Deserialize, Serialize};
pub use subc_config::{CanonicalError, Digest, InputVector, Plan};

pub const CONSENT_V1: &str = "consent/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ConsentProtocol {
    #[serde(rename = "consent/v1")]
    V1,
}

crate::config::open_string_enum! {
    /// Only an explicit approved answer can approve; future decisions fail closed.
    ConsentDecision { Approved => "approved", Declined => "declined" }
}
impl ConsentDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// Canonical plan view and exact approval binding, not a caller-composed summary.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsentRequest {
    protocol: ConsentProtocol,
    pub plan_digest: Digest,
    pub base_revision: u64,
    pub input_vector: InputVector,
    pub plan: Plan,
}
impl ConsentRequest {
    /// Builds the request from the plan alone, so its digest, revision and
    /// input vector always describe the plan the card shows.
    pub fn new(plan: Plan) -> Result<Self, CanonicalError> {
        Ok(Self {
            protocol: ConsentProtocol::V1,
            plan_digest: plan.digest()?,
            base_revision: plan.base_revision,
            input_vector: plan.input_vector.clone(),
            plan,
        })
    }

    /// Whether the binding fields describe the carried plan. A responder shows
    /// and answers only a consistent request, so a card can never display one
    /// plan while its approval binds another.
    pub fn is_consistent(&self) -> bool {
        self.plan
            .digest()
            .is_ok_and(|digest| digest == self.plan_digest)
            && self.base_revision == self.plan.base_revision
            && self.input_vector == self.plan.input_vector
    }
}

/// A response echoes every approval binding. Unknown decisions decode for
/// diagnostics but never approve. Decline, timeout or disconnect applies nothing.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentResponse {
    protocol: ConsentProtocol,
    pub plan_digest: Digest,
    pub base_revision: u64,
    pub input_vector: InputVector,
    pub decision: ConsentDecision,
}
impl ConsentResponse {
    pub fn new(request: &ConsentRequest, decision: ConsentDecision) -> Self {
        Self {
            protocol: ConsentProtocol::V1,
            plan_digest: request.plan_digest.clone(),
            base_revision: request.base_revision,
            input_vector: request.input_vector.clone(),
            decision,
        }
    }

    /// Check body binding only. The caller must additionally authenticate the
    /// response connection and the still-pending request before consuming it.
    pub fn approves(&self, request: &ConsentRequest) -> bool {
        self.decision.is_approved()
            && self.plan_digest == request.plan_digest
            && self.base_revision == request.base_revision
            && self.input_vector == request.input_vector
    }
}
