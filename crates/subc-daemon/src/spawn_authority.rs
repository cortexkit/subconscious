//! Spawn authority records: what the daemon attests about a supervised spawn
//! when that spawn opens a route (`caller` on `route.bind`) or serves one
//! (`target` on the `route.open` reply).
//!
//! The daemon keeps one [`SpawnAuthorityRecord`] per launch, created before the
//! child starts. It holds the spawn's identity (daemon incarnation, spawn
//! generation and module id), its program origin, its first-party mark, its
//! approved provider declarations, a monotonic authority epoch and whether the
//! child is still alive.
//!
//! The record dies at the first observation that the child exited, in the same
//! synchronous retirement step that voids the spawn's claims and shuts down its
//! registration channel (see `RegistrationSpawn::retire`). A dead record
//! attests nothing.
//!
//! # The epoch fence
//!
//! A route records the caller's and the target's `(spawn, epoch)` when the
//! open is attested. Any change to a spawn's authority bumps its epoch, and the
//! bind commit refuses a route whose captured epoch is older than the one the
//! forwarding table has published. Changes go through
//! [`SpawnAuthorityRecord::change_authority`], which publishes the new epoch and
//! selects the routes to close while it still holds the record's state.
//!
//! # Lock order
//!
//! 1. A spawn's retirement transition lock, then its record's authority state,
//!    then the launch's claim locks (retirement marks the record dead before it
//!    voids the claims).
//! 2. A record's authority state, then the forwarding table's write lock (an
//!    authority change).
//!
//! Code that holds the forwarding lock never takes a record's authority state:
//! it reads only the record's immutable identity and the epochs the forwarding
//! table itself has published. Nothing here is held across an await.
//!
//! Program origins are initialized from admission evidence before launch.
//! Requested declarations are frozen at HELLO; later approvals publish their
//! new set and epoch together. The mark itself is never raised here.

use std::sync::{Arc, Mutex, MutexGuard};

use sha2::{Digest, Sha256};
use subc_protocol::peer::{CallerAttestation, CallerClaim, CallerOrigin};
use subc_protocol::provider_declarations::ProviderDeclarations;

/// The epoch every record starts at. Only a change of authority advances it.
pub(crate) const INITIAL_AUTHORITY_EPOCH: u64 = 0;

/// A spawn's identity within one daemon incarnation. Spawn generations are
/// counted per module, so the module id is part of the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SpawnKey {
    pub(crate) module_id: String,
    pub(crate) spawn: u64,
}

/// A spawn and the authority epoch observed for it at one moment: what a
/// route records for its caller and its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorityPair {
    pub(crate) key: SpawnKey,
    pub(crate) epoch: u64,
}

/// The provider declarations a spawn may grant from, and their set digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovedDeclarations {
    /// Approved entries keyed by provider module id.
    pub(crate) entries: ProviderDeclarations,
    /// The digest of the whole approved set, lowercase hex.
    pub(crate) digest: String,
}

impl ApprovedDeclarations {
    /// Hash the empty set with the same algorithm used for requested declarations.
    pub(crate) fn none_approved() -> Self {
        let entries = ProviderDeclarations::new();
        let digest = declarations_digest(&entries).expect("the empty set is canonical JSON");
        Self { entries, digest }
    }
}

/// SHA-256 over the JSON Canonicalization Scheme (JCS) encoding of
/// {provider, entry}, then JCS({provider: entry_digest}).
/// Unlike config digests, these add no extra wrapping structure or prefix
/// beyond the JCS objects shown above.
pub(crate) fn entry_digest(
    provider: &str,
    entry: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    jcs_digest(&serde_json::json!({"provider": provider, "entry": entry}))
}

pub(crate) fn declarations_digest(entries: &ProviderDeclarations) -> Result<String, String> {
    let mut digests = serde_json::Map::new();
    for (provider, entry) in entries {
        digests.insert(
            provider.clone(),
            serde_json::Value::String(entry_digest(provider, entry)?),
        );
    }
    jcs_digest(&serde_json::Value::Object(digests))
}

fn jcs_digest(value: &serde_json::Value) -> Result<String, String> {
    let bytes = subc_config::canonical_json(value).map_err(|err| err.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Debug)]
struct AuthorityState {
    origin: CallerOrigin,
    hosts_third_party_code: bool,
    first_party: bool,
    epoch: u64,
    alive: bool,
    approved: ApprovedDeclarations,
    approved_digests: std::collections::BTreeMap<String, String>,
    requested: Option<ProviderDeclarations>,
}

/// One supervised spawn's authority record. See the module docs.
#[derive(Debug)]
pub(crate) struct SpawnAuthorityRecord {
    incarnation: String,
    spawn: u64,
    module_id: String,
    state: Mutex<AuthorityState>,
}

/// Everything an attestation needs, read from a live record in one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorityCapture {
    pub(crate) incarnation: String,
    pub(crate) key: SpawnKey,
    pub(crate) epoch: u64,
    pub(crate) first_party: bool,
    pub(crate) origin: CallerOrigin,
    pub(crate) approved: ApprovedDeclarations,
}

impl AuthorityCapture {
    pub(crate) fn pair(&self) -> AuthorityPair {
        AuthorityPair {
            key: self.key.clone(),
            epoch: self.epoch,
        }
    }
}

/// What the daemon attests about a route's opener: its spawn's authority,
/// captured once when the open is admitted, and the kind of connection it
/// opened the route on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallerCapture {
    pub(crate) authority: AuthorityCapture,
    pub(crate) claim: CallerClaim,
}

impl CallerCapture {
    /// The same caller, attested as acting for someone else. Used for a route
    /// opened under a federated or flow scope, whatever claim the connection
    /// used: the admitted scope, not the connection, says who the route acts
    /// for.
    pub(crate) fn relayed(mut self) -> Self {
        self.claim = CallerClaim::Relay;
        self
    }

    /// The `caller` key for `route.bind`. Only an `own` caller can carry the
    /// mark, and no declaration is sent while the mark is false. `None` only
    /// when an identity integer cannot be represented by every wire reader.
    pub(crate) fn attestation(&self) -> Option<CallerAttestation> {
        let first_party = self.authority.first_party && self.claim == CallerClaim::Own;
        CallerAttestation::new(
            self.authority.incarnation.clone(),
            self.authority.key.spawn,
            self.authority.epoch,
            self.claim.clone(),
            first_party,
            self.authority.origin,
            self.authority.approved.digest.clone(),
        )
        .ok()
    }
}

impl SpawnAuthorityRecord {
    /// A live, unmarked record for a spawn that is about to be launched.
    pub(crate) fn new(incarnation: impl Into<String>, spawn: u64, module_id: &str) -> Arc<Self> {
        Arc::new(Self {
            incarnation: incarnation.into(),
            spawn,
            module_id: module_id.to_owned(),
            state: Mutex::new(AuthorityState {
                origin: CallerOrigin::Other,
                hosts_third_party_code: true,
                first_party: false,
                epoch: INITIAL_AUTHORITY_EPOCH,
                alive: true,
                approved: ApprovedDeclarations::none_approved(),
                approved_digests: Default::default(),
                requested: None,
            }),
        })
    }

    pub(crate) fn spawn(&self) -> u64 {
        self.spawn
    }

    pub(crate) fn module_id(&self) -> &str {
        &self.module_id
    }

    pub(crate) fn approved(&self) -> ApprovedDeclarations {
        self.lock().approved.clone()
    }

    /// Called before the child starts, while no registration or route can read it.
    pub(crate) fn initialize_origin(
        &self,
        origin: CallerOrigin,
        digests: std::collections::BTreeMap<String, String>,
        hosts_third_party_code: Option<bool>,
    ) {
        let mut state = self.lock();
        state.origin = origin;
        state.hosts_third_party_code = hosts_third_party_code != Some(false);
        state.approved_digests = digests;
    }

    /// Program eligibility is distinct from readiness and stamping. Hosting
    /// placements retain their origin, but cannot receive the first-party mark.
    /// Missing hosting evidence is deliberately ineligible.
    pub(crate) fn origin_qualifies_for_mark(&self) -> bool {
        let state = self.lock();
        state.alive
            && matches!(
                state.origin,
                CallerOrigin::Development | CallerOrigin::Release
            )
            && !state.hosts_third_party_code
    }

    /// Freeze the spawn's request once. Matching saved digests approve only the
    /// exact requested entries, never an entry left over from another process.
    pub(crate) fn initialize_declarations(
        &self,
        requested: ProviderDeclarations,
    ) -> ProviderDeclarations {
        let mut state = self.lock();
        if !state.alive || state.requested.is_some() {
            return Default::default();
        }
        let mut differing = ProviderDeclarations::new();
        let mut entries = ProviderDeclarations::new();
        if state.origin == CallerOrigin::Development {
            for (provider, entry) in &requested {
                if entry_digest(provider, entry).ok().as_ref()
                    == state.approved_digests.get(provider)
                {
                    entries.insert(provider.clone(), entry.clone());
                } else {
                    differing.insert(provider.clone(), entry.clone());
                }
            }
        }
        state.approved = ApprovedDeclarations {
            digest: declarations_digest(&entries).expect("validated HELLO declarations"),
            entries,
        };
        state.requested = Some(requested);
        differing
    }

    /// Authority state precedes forwarding. Only entries
    /// frozen at this spawn's HELLO can enter the new approved set.
    pub(crate) fn approve_declarations<R>(
        &self,
        entries: &ProviderDeclarations,
        publish: impl FnOnce(u64, bool, &ApprovedDeclarations) -> R,
    ) -> Option<R> {
        let mut state = self.lock();
        if !state.alive || state.origin != CallerOrigin::Development {
            return None;
        }
        for (provider, entry) in entries {
            state.approved_digests.insert(
                provider.clone(),
                entry_digest(provider, entry).expect("validated declarations"),
            );
        }
        let entries = state
            .requested
            .as_ref()
            .into_iter()
            .flat_map(|requested| requested.iter())
            .filter(|(provider, entry)| {
                entry_digest(provider, entry).ok().as_ref() == state.approved_digests.get(*provider)
            })
            .map(|(provider, entry)| (provider.clone(), entry.clone()))
            .collect::<ProviderDeclarations>();
        if entries == state.approved.entries {
            return None;
        }
        state.approved = ApprovedDeclarations {
            digest: declarations_digest(&entries).expect("validated declarations"),
            entries,
        };
        state.epoch = state.epoch.saturating_add(1);
        Some(publish(state.epoch, state.alive, &state.approved))
    }

    pub(crate) fn key(&self) -> SpawnKey {
        SpawnKey {
            module_id: self.module_id.clone(),
            spawn: self.spawn,
        }
    }

    fn lock(&self) -> MutexGuard<'_, AuthorityState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Read every attested fact in one step. `None` once the record is dead:
    /// a dead spawn attests nothing.
    pub(crate) fn capture(&self) -> Option<AuthorityCapture> {
        let state = self.lock();
        state.alive.then(|| AuthorityCapture {
            incarnation: self.incarnation.clone(),
            key: self.key(),
            epoch: state.epoch,
            first_party: state.first_party,
            origin: state.origin,
            approved: state.approved.clone(),
        })
    }

    pub(crate) fn alive(&self) -> bool {
        self.lock().alive
    }

    #[cfg(test)]
    pub(crate) fn epoch_for_test(&self) -> u64 {
        self.lock().epoch
    }

    /// Start the retirement step: mark the record dead now and hold its state
    /// until the returned fence drops, which bumps the epoch. The caller voids
    /// the spawn's claims and shuts its channel while it holds the fence, so
    /// the four parts of retirement happen as one step. `None` when the record
    /// was already dead.
    pub(crate) fn begin_retirement(&self) -> Option<RetirementFence<'_>> {
        let mut state = self.lock();
        if !state.alive {
            return None;
        }
        state.alive = false;
        Some(RetirementFence { state })
    }

    /// Change this spawn's authority: bump its epoch and run `publish` with the
    /// new epoch and whether the spawn is still alive, while this record's
    /// state is held. `publish` takes the forwarding write lock to publish the
    /// epoch and select the routes to close, so the two happen in one critical
    /// section with the authority state taken first.
    pub(crate) fn change_authority<R>(&self, publish: impl FnOnce(u64, bool) -> R) -> R {
        let mut state = self.lock();
        state.epoch = state.epoch.saturating_add(1);
        publish(state.epoch, state.alive)
    }
}

/// Holds a dying record's state through the retirement step; dropping it
/// bumps the record's epoch, the step's last part.
#[must_use = "dropping the fence ends the retirement step"]
pub(crate) struct RetirementFence<'a> {
    state: MutexGuard<'a, AuthorityState>,
}

impl Drop for RetirementFence<'_> {
    fn drop(&mut self) {
        self.state.epoch = self.state.epoch.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_authority_record_starts_live_unmarked_and_unapproved() {
        let record = SpawnAuthorityRecord::new("incarnation", 7, "module");
        let capture = record.capture().expect("a new record attests");
        assert_eq!(capture.key.spawn, 7);
        assert_eq!(capture.key.module_id, "module");
        assert_eq!(capture.epoch, INITIAL_AUTHORITY_EPOCH);
        assert!(!capture.first_party);
        assert_eq!(capture.origin, CallerOrigin::Other);
        assert!(capture.approved.entries.is_empty());
        assert_eq!(
            capture.approved.digest,
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn spawn_authority_retirement_marks_dead_then_bumps_once() {
        let record = SpawnAuthorityRecord::new("incarnation", 1, "module");
        {
            let fence = record.begin_retirement().expect("first retirement");
            // Dead before the claims are voided, epoch still unchanged.
            drop(fence);
        }
        assert!(!record.alive());
        assert!(record.capture().is_none(), "a dead record attests nothing");
        assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH + 1);
        assert!(record.begin_retirement().is_none());
        assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH + 1);
    }

    #[test]
    fn spawn_authority_caller_attestation_is_never_first_party() {
        let record = SpawnAuthorityRecord::new("incarnation", 3, "module");
        for claim in [
            CallerClaim::Own,
            CallerClaim::Brokered,
            CallerClaim::Relay,
            CallerClaim::Legacy,
        ] {
            let caller = CallerCapture {
                authority: record.capture().unwrap(),
                claim: claim.clone(),
            }
            .attestation()
            .unwrap();
            assert!(!caller.first_party, "{claim:?}");
            assert_eq!(caller.claim, claim);
            assert!(caller.declaration.is_none());
            assert!(caller.declaration_digest.is_none());
        }
    }
}

#[cfg(test)]
mod development_approval_tests {
    use super::*;

    #[test]
    fn origins_widening_replaces_the_key_on_overlapping_and_unregistered_spawns() {
        let old: ProviderDeclarations =
            serde_json::from_value(serde_json::json!({"provider":{"item":"old"}})).unwrap();
        let new: ProviderDeclarations =
            serde_json::from_value(serde_json::json!({"provider":{"item":"new"}})).unwrap();
        let digests = std::collections::BTreeMap::from([(
            "provider".into(),
            entry_digest("provider", &old["provider"]).unwrap(),
        )]);
        let incumbent = SpawnAuthorityRecord::new("incarnation", 1, "module");
        incumbent.initialize_origin(CallerOrigin::Development, digests.clone(), Some(false));
        assert!(incumbent.initialize_declarations(old).is_empty());
        let candidate = SpawnAuthorityRecord::new("incarnation", 2, "module");
        candidate.initialize_origin(CallerOrigin::Development, digests.clone(), Some(false));
        assert_eq!(candidate.initialize_declarations(new.clone()), new);
        let unregistered = SpawnAuthorityRecord::new("incarnation", 3, "module");
        unregistered.initialize_origin(CallerOrigin::Development, digests, Some(false));
        for record in [&incumbent, &candidate] {
            assert_eq!(
                record.approve_declarations(&new, |epoch, alive, _| (epoch, alive)),
                Some((1, true))
            );
        }
        assert!(incumbent.capture().unwrap().approved.entries.is_empty());
        assert_eq!(candidate.capture().unwrap().approved.entries, new);
        assert!(unregistered
            .approve_declarations(&new, |_, _, _| ())
            .is_none());
        assert!(unregistered.initialize_declarations(new.clone()).is_empty());
        assert_eq!(unregistered.capture().unwrap().approved.entries, new);
    }
}
