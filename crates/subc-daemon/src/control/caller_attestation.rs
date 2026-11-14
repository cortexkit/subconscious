//! What `route.open` attests about its opener and its target, read from the
//! spawn authority records (`crate::spawn_authority`).
//!
//! The opener is resolved from its connection: a claim-bound connection from
//! its claim binding, an older SDK's consumer connection from its launch
//! nonce. Either names a launch, and its authority record is captured once per
//! open and carried into the pending relay, never read again.

use super::*;
use crate::spawn_authority::{CallerCapture, SpawnAuthorityRecord};
use subc_control::ClaimPurpose;
use subc_protocol::peer::{CallerAttestation, CallerClaim, CALLER_ATTESTATION_CAPABILITY};

/// What a `route.open`'s connection and `consumer_identity` attest about the
/// opener.
#[derive(Debug, Clone)]
pub(super) struct ConsumerAttestation {
    /// The principal the route is opened as.
    pub(super) principal: Principal,
    /// The launch behind the opener and the kind of connection it used.
    /// `None` for a `direct` caller (the CLI, an app, an unsupervised client),
    /// and for a launch-nonce caller whose launch keeps no record.
    pub(super) spawn: Option<(Arc<SpawnAuthorityRecord>, CallerClaim)>,
}

impl ConsumerAttestation {
    fn direct() -> Self {
        Self {
            principal: Principal::Direct,
            spawn: None,
        }
    }

    /// Read the opener's authority once. `Err` when its record has died since
    /// the connection was attested: a dead spawn attests nothing.
    pub(super) fn capture(&self) -> Result<Option<CallerCapture>, ()> {
        match &self.spawn {
            None => Ok(None),
            Some((record, claim)) => record
                .capture()
                .map(|authority| {
                    Some(CallerCapture {
                        authority,
                        claim: claim.clone(),
                    })
                })
                .ok_or(()),
        }
    }
}

/// The `caller` key for a bind toward `target_module`, a target with
/// `capabilities`: present only for a supervised opener and only toward a
/// target declaring `caller-attestation/v1`.
pub(super) fn caller_for_target(
    caller: Option<&CallerCapture>,
    capabilities: Option<&CapabilityDeclarations>,
    target_module: &str,
) -> Option<CallerAttestation> {
    let declared = capabilities.is_some_and(|caps| {
        caps.provides
            .iter()
            .any(|cap| cap == CALLER_ATTESTATION_CAPABILITY)
    });
    caller
        .filter(|_| declared)
        .and_then(|caller| caller.attestation(target_module))
}

/// Whether a route admitted under `stamp` acts for someone other than the
/// module that carries it: a federated scope (another computer or a phone) or
/// a flow scope (the flow's code). Read from the admitted stamp, before any
/// field is stripped for an older target.
pub(super) fn scope_acts_for_another(stamp: &subc_protocol::scope::ScopeStamp) -> bool {
    stamp.kind == subc_protocol::scope::ScopeKind::Federated || stamp.attributes.flow_id.is_some()
}

/// Publishes first-party mark changes the configuration gate decides. It runs
/// while the gate holds its delivery state, takes the spawn's record state and
/// then the forwarding write lock, and leaves the catalog notice and route
/// closes to a task that runs once every one of those locks is released.
pub(super) struct ControlMarkPublisher(pub(super) ControlHandler);

impl ControlMarkPublisher {
    fn after_locks(&self, work: impl FnOnce(&ControlHandler) + Send + 'static) {
        let handler = self.0.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move { work(&handler) });
            }
            // Never run it inline: the caller still holds the gate's state.
            Err(_) => {
                std::thread::spawn(move || work(&handler));
            }
        }
    }
}

impl crate::config_gate::runtime::mark::MarkPublisher for ControlMarkPublisher {
    fn activate(&self, record: &Arc<SpawnAuthorityRecord>) -> bool {
        let key = record.key();
        match record.activate(|| self.0.forwarding.publish_spawn_mark(&key)) {
            Ok(true) => {
                let record = record.clone();
                self.after_locks(move |handler| {
                    if let Err(err) = handler.registry.catalog_authority_changed() {
                        warn!(module_id = record.module_id(), spawn = record.spawn_number(), error = %err,
                            "failed to advance the catalog after a first-party mark activation");
                    }
                });
                true
            }
            Ok(false) => false,
            Err(err) => {
                warn!(module_id = record.module_id(), spawn = record.spawn_number(), error = %err,
                    "failed to publish a first-party mark activation");
                false
            }
        }
    }

    fn lower(&self, record: &Arc<SpawnAuthorityRecord>) {
        let key = record.key();
        let Some(published) = record.lower(|change| {
            self.0
                .forwarding
                .publish_authority_change(&key, change.epoch, change.alive, change.first_party)
                .map(|closed| (closed, change.alive))
        }) else {
            return;
        };
        match published {
            Ok((closed, alive)) => {
                let record = record.clone();
                self.after_locks(move |handler| {
                    handler.after_spawn_authority_published(&record, closed, alive);
                });
            }
            Err(err) => {
                warn!(module_id = record.module_id(), spawn = record.spawn_number(), error = %err,
                    "failed to publish a first-party mark lowering");
            }
        }
    }
}

impl ControlHandler {
    /// Attest a `route.open`'s opener. `None` when it claims an identity it
    /// cannot prove: a launch nonce that does not verify, a claim binding
    /// whose launch has ended, or any `consumer_identity` supplied alongside
    /// a binding. A claim-bound connection is attested by its binding
    /// alone; its principal is always `reserved:<the binding's module>`.
    pub(super) fn attest_route_open_consumer(
        &self,
        connection: ConnectionId,
        consumer_identity: Option<&ConsumerIdentity>,
    ) -> Option<ConsumerAttestation> {
        if let Some(binding) = self.claims.binding(connection) {
            let registration = &binding.registration;
            let claim = match &binding.purpose {
                ClaimPurpose::SelfConnection => CallerClaim::Own,
                ClaimPurpose::Helper => CallerClaim::Brokered,
                ClaimPurpose::Relay => CallerClaim::Relay,
                // A prefix child's connection never opens routes; any purpose
                // added later attests nothing until a rule for it exists.
                _ => return None,
            };
            if consumer_identity.is_some()
                || !binding.consumer_live(connection)
                || !registration.authority.alive()
            {
                return None;
            }
            return Some(ConsumerAttestation {
                principal: Principal::Reserved {
                    module_id: registration.module_id.clone(),
                },
                spawn: Some((registration.authority.clone(), claim)),
            });
        }
        let Some(consumer_identity) = consumer_identity else {
            return Some(ConsumerAttestation::direct());
        };
        self.attest_nonce_consumer(Some(consumer_identity))
            .map(|principal| ConsumerAttestation {
                principal,
                spawn: self
                    .supervisor
                    .spawned_consumer_record(
                        &consumer_identity.module_id,
                        &consumer_identity.launch_nonce,
                    )
                    .map(|record| (record, CallerClaim::Legacy)),
            })
    }

    /// The registration whose manifest the caller-side `route.open` checks
    /// read: the opening spawn's own registration (a warming candidate's or a
    /// draining incumbent's included) when the opener has a spawn record, and
    /// the module's active registration otherwise.
    pub(super) fn caller_registration(
        &self,
        attested: &ConsumerAttestation,
        admission_registry: &[crate::registry::ModuleRegistration],
    ) -> Option<crate::registry::ModuleRegistration> {
        let Principal::Reserved { module_id } = &attested.principal else {
            return None;
        };
        if let Some((record, _)) = &attested.spawn {
            let own = self
                .forwarding
                .connection_for_spawn(&record.key())
                .ok()
                .flatten()
                .and_then(|connection| self.registry.get_module_by_connection(connection).ok())
                .flatten();
            if own.is_some() {
                return own;
            }
        }
        admission_registry
            .iter()
            .find(|registration| registration.manifest.module_id == *module_id)
            .cloned()
    }

    /// Start a spawn authority change: bump the spawn's epoch, publish it and
    /// close every route that recorded the spawn at an older epoch, as caller
    /// or target. The record's state is taken first, then the forwarding
    /// table. A live spawn's routes close with `authority_changed`; a dead
    /// spawn's keep the crash reason, because its child exited.
    pub(super) fn change_spawn_authority(&self, record: &SpawnAuthorityRecord) {
        let key = record.key();
        let published = record.change_authority(|change| {
            self.forwarding
                .publish_authority_change(&key, change.epoch, change.alive, change.first_party)
                .map(|closed| (closed, change.alive))
        });
        self.finish_spawn_authority_change(record, published);
    }

    pub(super) fn approve_spawn_declarations(
        &self,
        record: &SpawnAuthorityRecord,
        entries: &subc_protocol::provider_declarations::ProviderDeclarations,
    ) {
        let key = record.key();
        let published = record.approve_declarations(entries, |epoch, alive, approved| {
            self.forwarding
                .publish_approved_authority_change(&key, epoch, alive, None, Some(approved))
                .map(|closed| (closed, alive))
        });
        if let Some(published) = published {
            self.finish_spawn_authority_change(record, published);
        }
    }

    fn finish_spawn_authority_change(
        &self,
        record: &SpawnAuthorityRecord,
        published: Result<(Vec<crate::forwarding::AuthorityClosedRoute>, bool), ForwardingError>,
    ) {
        let (closed, alive) = match published {
            Ok(published) => published,
            Err(err) => {
                warn!(module_id = record.module_id(), spawn = record.spawn_number(), error = %err,
                    "failed to publish a spawn authority change");
                return;
            }
        };
        // Authority and forwarding locks have been released. Separate watch
        // tasks send the notifications, so a slow client cannot hold up fencing.
        self.after_spawn_authority_published(record, closed, alive);
    }

    /// The work that follows a published authority change once every
    /// authority and forwarding lock is released: advance the catalog, then
    /// close each selected route with its recipient's reason. A live spawn's
    /// routes close with `authority_changed` where the opener declared it; a
    /// dead spawn's keep the crash reason, because its child exited.
    pub(super) fn after_spawn_authority_published(
        &self,
        record: &SpawnAuthorityRecord,
        closed: Vec<crate::forwarding::AuthorityClosedRoute>,
        alive: bool,
    ) {
        if let Err(err) = self.registry.catalog_authority_changed() {
            warn!(module_id = record.module_id(), spawn = record.spawn_number(), error = %err,
                "failed to advance the catalog after a spawn authority change");
        }
        if closed.is_empty() {
            return;
        }
        let reason = if alive {
            RouteCloseReason::AuthorityChanged
        } else {
            RouteCloseReason::Crash
        };
        let mut pushes: BTreeMap<String, Vec<EndpointRoute>> = BTreeMap::new();
        let mut goodbyes = Vec::with_capacity(closed.len() * 2);
        for closed in closed {
            warn!(module_id = %closed.route.module_id, reason = ?reason,
                client_connection_id = closed.client.connection_id.get(),
                route_channel = closed.client.channel,
                "closing route because a spawn's authority changed");
            pushes
                .entry(closed.route.module_id.clone())
                .or_default()
                .push(EndpointRoute {
                    goodbye_target: closed.client.clone(),
                    principal: closed.route.principal.clone(),
                    bound_at: closed.route.bound_at,
                    draining: false,
                    drain_reason: None,
                    accepts_close_reasons: closed.route.accepts_close_reasons.clone(),
                });
            goodbyes.push(closed.module);
            goodbyes.push(closed.client);
        }
        for (module_id, routes) in pushes {
            send_route_control_pushes(
                &self.forwarding,
                routes,
                ClientControlPush::RouteClosed {
                    module_id,
                    channels: Vec::new(),
                    reason,
                    drained: false,
                    abandoned: 0,
                    excluded_subscriptions: 0,
                    terminal: Some(false),
                },
            );
        }
        self.emit_route_goodbyes(goodbyes);
    }
}
