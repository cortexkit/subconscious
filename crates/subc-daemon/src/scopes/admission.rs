//! Pure route admission over one scope-table view and one registry snapshot.

use super::*;
use crate::registry::{ModuleRegistration, RouteAdmissionSnapshot};
use subc_protocol::{
    manifest::ModuleManifest,
    scope::{RouteAuthorizeExpected, RouteAuthorizeOutcome as Outcome, RouteAuthorizeResponse},
};

pub(crate) enum AdmissionMode<'a> {
    Open,
    Authorize(&'a RouteAuthorizeExpected),
}

pub(crate) struct RouteAdmission {
    pub(crate) scope: Option<ScopeAdmission>,
    pub(crate) content_version: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) outcome: Outcome,
    pub(crate) code: Option<&'static str>,
    pub(crate) message: String,
}

impl From<ScopeAdmissionRefusal> for Refusal {
    fn from(refusal: ScopeAdmissionRefusal) -> Self {
        let outcome = match refusal.code {
            error_codes::SCOPE_NOT_SYNCED => Outcome::Indeterminate,
            error_codes::SCOPE_NOT_LIVE => Outcome::ScopeEnded,
            error_codes::SCOPE_ENDED => Outcome::EpochChanged,
            error_codes::SCOPE_BIND_PROOF_REQUIRED | error_codes::SCOPE_BIND_PROOF_MISMATCH => {
                Outcome::ProofRequired
            }
            _ => Outcome::NotAuthorized,
        };
        Self {
            outcome,
            code: Some(refusal.code),
            message: refusal.message,
        }
    }
}

impl Refusal {
    fn only(outcome: Outcome, message: &str) -> Self {
        Self {
            outcome,
            code: None,
            message: message.into(),
        }
    }

    pub(crate) fn response(self) -> RouteAuthorizeResponse {
        let mut response = RouteAuthorizeResponse::new(self.outcome, self.message);
        response.route_open_code = self.code.map(str::to_string);
        response
    }
}

/// Target availability is separate from transport state: an authorization
/// query neither reserves egress nor promises that a provider will answer.
pub(crate) fn target_registration<'a>(
    registry: &'a [ModuleRegistration],
    target: &str,
) -> Result<&'a ModuleRegistration, Refusal> {
    let registration = registered_target(registry, target)
        .ok_or_else(|| Refusal::only(Outcome::TargetAbsent, "target is not registered"))?;
    if !registration.ready {
        return Err(Refusal::only(Outcome::TargetAbsent, "target is not ready"));
    }
    Ok(registration)
}

pub(crate) fn registered_target<'a>(
    registry: &'a [ModuleRegistration],
    target: &str,
) -> Option<&'a ModuleRegistration> {
    registry.iter().find(|r| r.manifest.module_id == target)
}

fn check_target_published(
    registry: &RouteAdmissionSnapshot,
    target: &ModuleRegistration,
) -> Result<(), Refusal> {
    if registry.unpublished.contains(&target.connection_id) {
        return Err(Refusal::only(
            Outcome::TargetAbsent,
            "target is still registering",
        ));
    }
    Ok(())
}

fn principal_registration<'a>(
    registry: &'a [ModuleRegistration],
    principal: &Principal,
) -> Option<&'a ModuleRegistration> {
    reserved_module_id(principal)
        .and_then(|id| registry.iter().find(|r| r.manifest.module_id == id))
}

/// Find the opener's first denied capability claimed by the target. Both
/// manifests are attested registrations, never facts supplied by a route opener.
pub(crate) fn denied_capability<'a>(
    opening: &'a ModuleManifest,
    target: &ModuleManifest,
) -> Option<&'a str> {
    let denied = &opening.capabilities.as_ref()?.must_never_reach;
    let provides = &target.capabilities.as_ref()?.provides;
    denied
        .iter()
        .find(|capability| provides.contains(capability))
        .map(String::as_str)
}

pub(crate) fn check_deny_edge(
    registry: &[ModuleRegistration],
    principal: &Principal,
    target: &ModuleRegistration,
) -> Result<(), ScopeAdmissionRefusal> {
    if let Some(opening) = principal_registration(registry, principal) {
        if let Some(capability) = denied_capability(&opening.manifest, &target.manifest) {
            return Err(ScopeAdmissionRefusal {
                code: "capability_forbidden",
                message: format!(
                    "module_id '{}' must never reach capability '{capability}' provided by '{}'",
                    opening.manifest.module_id, target.manifest.module_id
                ),
            });
        }
    }
    Ok(())
}

fn check_stamp_support(
    admission: &ScopeAdmission,
    target: &ModuleRegistration,
) -> Result<(), ScopeAdmissionRefusal> {
    let id = &target.manifest.module_id;
    let capabilities = target.manifest.capabilities.as_ref();
    check_target_required_stamp(admission.stamp_required, id, capabilities)?;
    check_target_flow_support(&admission.stamp, id, capabilities)?;
    check_target_agent_run_support(&admission.stamp, id, capabilities)
}

/// Both callers use the same predicates. Open keeps its historical precedence
/// (target, deny edge, scope/carrier, proof, stamp capabilities). Authorize puts
/// retryable owner/scope facts first, so a fact true only until an owner re-syncs
/// cannot produce a terminal answer. Parent and expected-claim checks are
/// deliberately stricter for stored calls than for ordinary opens. Authorize
/// also rejects an owner removed from configuration: it can never serve a
/// deferred call, whereas Open preserves admission of its already-synced scopes.
/// A staged target is absent to Authorize until publication; Open preserves its
/// existing forwarding-availability check rather than adding a publication gate.
/// When those authorize-only facts hold, the principal is registered and the
/// target is published,
/// Authorize succeeds exactly when Open's shared checks admit. Open additionally
/// checks role and transient transport/availability facts outside this function.
pub(crate) fn admit_route(
    scopes: &ScopeTable,
    registry: &RouteAdmissionSnapshot,
    principal: &Principal,
    target: &str,
    selector: Option<&ScopeSelector>,
    owner_configured: bool,
    mode: AdmissionMode<'_>,
) -> Result<RouteAdmission, Refusal> {
    match mode {
        AdmissionMode::Open => {
            let registration = target_registration(registry, target)?;
            check_deny_edge(registry, principal, registration)?;
            let scope = selector
                .map(|selector| {
                    let admission = scopes.admit(principal, target, selector, owner_configured)?;
                    check_stamp_support(&admission, registration)?;
                    Ok::<_, ScopeAdmissionRefusal>(admission)
                })
                .transpose()?;
            Ok(RouteAdmission {
                scope,
                content_version: None,
            })
        }
        AdmissionMode::Authorize(expected) => {
            let selector = selector.expect("authorize always pins a scope");
            if reserved_module_id(&selector.owner).is_none() || !owner_configured {
                return Err(ScopeAdmissionRefusal {
                    code: error_codes::SCOPE_NOT_LIVE,
                    message: "scope owner is not a configured supervised module".into(),
                }
                .into());
            }
            let (owner, scope) = scopes.resolve(selector, owner_configured)?;
            match scope.parent_state {
                Some(ParentState::Pending) => {
                    return Err(Refusal::only(
                        Outcome::Indeterminate,
                        "parent link is pending",
                    ))
                }
                Some(ParentState::Ended) => {
                    return Err(Refusal::only(Outcome::ScopeEnded, "parent link has ended"))
                }
                _ => {}
            }
            if !scope.owner_authorized
                || *expected
                    != RouteAuthorizeExpected::new(scope.record.kind, &scope.record.attributes)
            {
                return Err(Refusal::only(
                    Outcome::ClaimChanged,
                    "scope claim changed or its owner is not authorized",
                ));
            }
            scopes.check_bind_proof(principal, selector)?;
            scopes.check_carrier(scope, owner, principal, target)?;
            if reserved_module_id(principal).is_some()
                && principal_registration(registry, principal).is_none()
            {
                return Err(Refusal::only(
                    Outcome::Indeterminate,
                    "principal module is not registered",
                ));
            }
            let registration = target_registration(registry, target)?;
            check_target_published(registry, registration)?;
            check_deny_edge(registry, principal, registration)?;
            let admission = ScopeTable::admission_stamp(owner, scope, selector);
            check_stamp_support(&admission, registration)?;
            Ok(RouteAdmission {
                scope: Some(admission),
                content_version: Some(content_version(scope, selector, principal)),
            })
        }
    }
}

fn content_version(scope: &LiveScope, selector: &ScopeSelector, principal: &Principal) -> String {
    let mut attributes =
        serde_json::to_value(&scope.record.attributes).expect("serializable scope attributes");
    if let Some(federated) = attributes.get_mut("federated") {
        if let Some(claim) = federated
            .get_mut("remote_claim")
            .and_then(serde_json::Value::as_object_mut)
        {
            claim.remove("agent_name");
            claim.remove("flow_name");
        }
    }
    canonicalize(&mut attributes);
    let allowance = if reserved_module_id(principal) == reserved_module_id(&selector.owner) {
        Some(None)
    } else {
        carrier_allowance(&scope.record, principal)
    };
    let allowance = match allowance {
        Some(None) => serde_json::json!("any"),
        Some(Some(targets)) => serde_json::json!(targets),
        None => serde_json::Value::Null,
    };
    // A JSON tuple fixes field order and preserves boundaries and absent values.
    // Attribute objects are ordered maps; carrier targets are a sorted set.
    let bytes = serde_json::to_vec(&(
        "subc.route.authorize.content/v1",
        &selector.owner,
        &selector.scope_ref,
        scope.record.scope_epoch,
        scope.record.kind,
        attributes,
        scope.owner_authorized,
        &scope.record.parent,
        scope.parent_state,
        &scope.record.bind_proof_sha256,
        scope.record.expires_at_ms,
        allowance,
    ))
    .expect("serializable scope content");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn canonicalize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(canonicalize);
            map.sort_keys();
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalize),
        _ => {}
    }
}

#[cfg(test)]
mod tests;
