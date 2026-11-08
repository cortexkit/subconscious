//! Trusted installation boundary between reviewed config and live route authority.

use std::{future::Future, pin::Pin, sync::Arc};

use super::*;

/// Exact stored inputs for one approved transition. This value cannot be
/// deserialized from a control request or assembled by an untrusted module.
#[derive(Clone)]
pub struct ApprovedRoutePolicy {
    module: String,
    generation: u64,
    digest: Digest,
    changeset: Changeset,
    delivery: DeliverySnapshot,
}

impl ApprovedRoutePolicy {
    pub fn module(&self) -> &str {
        &self.module
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn plan_digest(&self) -> &Digest {
        &self.digest
    }
    pub fn changeset(&self) -> &Changeset {
        &self.changeset
    }
    pub fn delivery(&self) -> &DeliverySnapshot {
        &self.delivery
    }
}

pub type RoutePolicyInstall<'a> =
    Pin<Box<dyn Future<Output = Result<RoutePolicyInstallation>> + Send + 'a>>;

/// In-memory policy restoration captured by the trusted route adapter. Both
/// operations are infallible, bounded in-memory changes; they cannot await,
/// perform I/O, or reacquire the configuration store or module lock. Restoring
/// policy does not resurrect closed routes or their revoked permission stamps.
pub trait RoutePolicyReceipt: Send {
    fn commit(&mut self);
    fn restore(&mut self);
}

/// Owned reversible installation. Dropping an unresolved installation restores
/// its captured policy, including a pre-gate policy absent from store history.
pub struct RoutePolicyInstallation {
    receipt: Option<Box<dyn RoutePolicyReceipt>>,
}

impl RoutePolicyInstallation {
    pub fn new(receipt: Box<dyn RoutePolicyReceipt>) -> Self {
        Self {
            receipt: Some(receipt),
        }
    }
    pub fn commit(&mut self) {
        if let Some(mut receipt) = self.receipt.take() {
            receipt.commit();
        }
    }
    pub fn restore(&mut self) {
        if let Some(mut receipt) = self.receipt.take() {
            receipt.restore();
        }
    }
}

impl Drop for RoutePolicyInstallation {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The daemon's route-policy adapter, supplied through its Rust constructor.
/// Success means affected routes and the stamps granting their permissions
/// have been revoked, and every new route.open reads the exact approved policy.
/// The delivery caller holds the module configuration lock, but not the store
/// mutex, across this call. An implementation must not reacquire that lock or
/// report success after merely requesting an asynchronous policy reload.
/// Its future must retain a restoration guard before changing authority, so
/// cancellation also restores the previous policy. Daemon restart rebuilds
/// authority from the durable active generation, or from the configuration files
/// used before approval was enabled when no active generation has been recorded.
pub trait RoutePolicyAuthority: Send + Sync {
    fn install_and_revoke<'a>(&'a self, policy: ApprovedRoutePolicy) -> RoutePolicyInstall<'a>;
}

pub(super) struct PendingRoutePolicy {
    installation: RoutePolicyInstallation,
    previous_generation: Option<u64>,
}

impl OwnerState {
    pub(super) fn restore_route_policy(&mut self, module: &str, generation: u64) {
        if let Some(mut pending) = self
            .route_policy_receipts
            .remove(&(module.into(), generation))
        {
            pending.installation.restore();
            if let Some(previous) = pending.previous_generation {
                self.installed_route_policies
                    .insert(module.into(), previous);
            } else {
                self.installed_route_policies.remove(module);
            }
        }
    }

    pub(super) fn commit_route_policy(&mut self, module: &str, generation: u64) {
        if let Some(mut pending) = self
            .route_policy_receipts
            .remove(&(module.into(), generation))
        {
            pending.installation.commit();
        }
    }

    pub(super) fn commit_anchored_route_policies(&mut self) {
        if self.store.status().anchor_pending {
            return;
        }
        let active = self
            .store
            .body()
            .map(|body| {
                body.versions
                    .iter()
                    .filter_map(|(tuple, versions)| {
                        versions
                            .active
                            .map(|generation| (tuple.module.clone(), generation))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (module, generation) in active {
            self.commit_route_policy(&module, generation);
        }
    }
}

pub(super) fn restore_superseded(
    receipts: &mut BTreeMap<(String, u64), PendingRoutePolicy>,
    installed: &mut BTreeMap<String, u64>,
    modules: &BTreeSet<String>,
    delivery: &mut delivery::DeliveryState,
) {
    let pending: Vec<_> = receipts
        .keys()
        .filter(|(module, _)| modules.contains(module))
        .cloned()
        .collect();
    for (module, generation) in pending {
        if let Some(mut pending) = receipts.remove(&(module.clone(), generation)) {
            pending.installation.restore();
            if let Some(previous) = pending.previous_generation {
                installed.insert(module.clone(), previous);
            } else {
                installed.remove(&module);
            }
            delivery.invalidate_policy(&module, generation);
        }
    }
}

pub(super) fn required(changeset: &Changeset, module: &str) -> bool {
    changeset.fields.iter().any(|field| {
        field.module_id == module && field.apply == subc_config::ApplyKind::RouteRebind
    })
}

pub(super) fn ready(state: &mut OwnerState, module: &str, generation: u64) -> Result<bool> {
    let required = state
        .store
        .body()?
        .generations
        .get(&generation)
        .is_some_and(|entry| required(&entry.generation.changeset, module));
    Ok(!required || state.installed_route_policies.get(module) == Some(&generation))
}

impl RuntimeOwner {
    pub fn with_route_policy_authority(mut self, authority: Arc<dyn RoutePolicyAuthority>) -> Self {
        self.route_policy_authority = Some(authority);
        self
    }

    pub(super) fn check_route_policy_authority(&self, changeset: &Changeset) -> Result<()> {
        if self.route_policy_authority.is_none()
            && changeset
                .fields
                .iter()
                .any(|field| field.apply == subc_config::ApplyKind::RouteRebind)
        {
            return Err(GateError::new("route_policy_authority_unavailable",
                "config_gated: RoutePolicyAuthority is absent; approved route policy cannot be installed and affected routes and stamps cannot be revoked"));
        }
        Ok(())
    }

    /// Called under the module configuration lock after an admitted replacement
    /// starts, if this generation requires one. Only saved approved bytes are
    /// passed to the policy adapter.
    pub(crate) async fn install_approved_route_policy(
        &self,
        module: &str,
        generation: u64,
    ) -> Result<()> {
        let authority = self.route_policy_authority.as_ref().ok_or_else(|| {
            GateError::new(
                "route_policy_authority_unavailable",
                "config_gated: RoutePolicyAuthority is absent",
            )
        })?;
        let (policy, previous_generation) = {
            let mut state = self.lock()?;
            state.store.delivery_ready()?;
            if ready(&mut state, module, generation)? {
                return Ok(());
            }
            let body = state.store.body()?;
            if !body.versions.iter().any(|(tuple, versions)| {
                tuple.module == module
                    && tuple.project.is_none()
                    && (versions.pending == Some(generation) || versions.active == Some(generation))
            }) {
                return Err(GateError::new(
                    "delivery_superseded",
                    "route policy generation is no longer pending",
                ));
            }
            let previous_generation = body
                .versions
                .iter()
                .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
                .and_then(|(_, versions)| versions.active);
            let entry = body.generations.get(&generation).ok_or_else(|| {
                GateError::new("config_gated", "approved route policy generation is absent")
            })?;
            (
                ApprovedRoutePolicy {
                    module: module.into(),
                    generation,
                    digest: entry.generation.plan_digest.clone(),
                    changeset: entry.generation.changeset.clone(),
                    delivery: entry.delivery.clone().ok_or_else(|| {
                        GateError::new("config_gated", "approved route policy bytes are absent")
                    })?,
                },
                previous_generation,
            )
        };
        let installation = authority.install_and_revoke(policy).await?;
        {
            let mut state = self.lock()?;
            if !state
                .store
                .body()?
                .versions
                .iter()
                .any(|(tuple, versions)| {
                    tuple.module == module
                        && tuple.project.is_none()
                        && (versions.pending == Some(generation)
                            || versions.active == Some(generation))
                })
            {
                return Err(GateError::new(
                    "delivery_superseded",
                    "route policy generation was superseded during installation",
                ));
            }
            let target = state.active_declarations.get(module).copied();
            state
                .delivery
                .policy_installed(module, generation, target)?;
            state
                .installed_route_policies
                .insert(module.into(), generation);
            state.route_policy_receipts.insert(
                (module.into(), generation),
                PendingRoutePolicy {
                    installation,
                    previous_generation,
                },
            );
            state.commit_anchored_route_policies();
        }
        self.notify_delivery_changed();
        Ok(())
    }

    pub(crate) fn route_policy_needed(&self, module: &str, generation: u64) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(!ready(&mut state, module, generation)?)
    }
}
