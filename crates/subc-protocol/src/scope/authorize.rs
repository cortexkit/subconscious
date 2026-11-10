use serde::{Deserialize, Serialize};

use super::{deserialize_scope_principal, ScopeAttributes, ScopeKind};
use crate::Principal;

/// Only the configured federated owner may provide this authorization oracle.
pub const ROUTE_AUTHORIZE_CAPABILITY: &str = "route-authorize/v1";
pub const ROUTE_AUTHORIZE_OP: &str = "route.authorize";

/// A stored scope identity, never a bind proof or an unpinned selector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RouteAuthorizeScope {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
}

impl RouteAuthorizeScope {
    pub fn new(owner: Principal, scope_ref: impl Into<String>, epoch: u64) -> Self {
        Self {
            owner,
            scope_ref: scope_ref.into(),
            epoch,
        }
    }
}

/// Claim derivation inputs. Missing fields must also be absent on the scope;
/// they are not wildcards. Like a stamp, omitted `delegates` means false.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RouteAuthorizeExpected {
    pub kind: ScopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delegates: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_code_hash: Option<String>,
}

impl RouteAuthorizeExpected {
    pub fn new(kind: ScopeKind, attributes: &ScopeAttributes) -> Self {
        Self {
            kind,
            agent_id: attributes.agent_id.clone(),
            delegates: attributes.delegates,
            run_id: attributes.run_id.clone(),
            on_behalf_of_agent: attributes.on_behalf_of_agent.clone(),
            flow_id: attributes.flow_id.clone(),
            flow_version: attributes.flow_version,
            flow_install_id: attributes.flow_install_id.clone(),
            flow_code_hash: attributes.flow_code_hash.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteAuthorizeOutcome {
    /// Not a promise that a later open succeeds: transient availability checks
    /// can still refuse it without sending a bind to the provider.
    Authorized,
    ScopeEnded,
    Indeterminate,
    EpochChanged,
    ClaimChanged,
    ProofRequired,
    NotAuthorized,
    TargetAbsent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct RouteAuthorizeResponse {
    op: RouteAuthorizeOp,
    pub outcome: RouteAuthorizeOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_open_code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum RouteAuthorizeOp {
    #[serde(rename = "route.authorize")]
    Authorize,
}

impl RouteAuthorizeResponse {
    pub fn new(outcome: RouteAuthorizeOutcome, message: impl Into<String>) -> Self {
        Self {
            op: RouteAuthorizeOp::Authorize,
            outcome,
            content_version: None,
            route_open_code: None,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn route_authorize_expected_is_closed_and_absence_is_not_a_wildcard() {
        let expected: RouteAuthorizeExpected =
            serde_json::from_value(json!({"kind":"head"})).unwrap();
        assert_ne!(
            expected,
            RouteAuthorizeExpected::new(
                ScopeKind::Head,
                &ScopeAttributes::new().with_agent_id(Some("agent".into()))
            )
        );
        assert!(serde_json::from_value::<RouteAuthorizeExpected>(
            json!({"kind":"head","agent_name":"display"})
        )
        .is_err());
        let response = serde_json::to_value(RouteAuthorizeResponse::new(
            RouteAuthorizeOutcome::ClaimChanged,
            "changed",
        ))
        .unwrap();
        assert_eq!(
            response,
            json!({"op":"route.authorize","outcome":"claim_changed","message":"changed"})
        );
    }
}
