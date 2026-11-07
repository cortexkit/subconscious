//! Shared consent/v1 plan card bodies for the daemon requester and responders.
//!
//! A reply can approve only the pending request on the daemon-owned connection,
//! after that connection's trust and active registration are rechecked. Matching
//! these body fields is necessary, never sufficient authority. Rendering a card
//! or asserting that a terminal showed a diff is not approval.

use serde::{Deserialize, Serialize};
pub use subc_config::{Digest, InputVector, Plan};

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
    pub fn new(plan_digest: Digest, plan: Plan) -> Self {
        Self {
            protocol: ConsentProtocol::V1,
            plan_digest,
            base_revision: plan.base_revision,
            input_vector: plan.input_vector.clone(),
            plan,
        }
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
