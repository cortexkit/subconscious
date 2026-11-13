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
//!    authority change, an activation or a lowering).
//! 3. The configuration gate's delivery state, then a record's authority
//!    state: the gate decides activation and lowering while it holds its own
//!    state, so the whole order is delivery, record, forwarding.
//!
//! Code that holds the forwarding lock never takes a record's authority state:
//! it reads only the record's immutable identity and the epochs and marks the
//! forwarding table itself has published. Nothing here is held across an
//! await. Debug builds check both rules where the owner's state and a
//! record's state are locked (`forwarding::assert_forwarding_unlocked`). The
//! one piece of code that runs under the forwarding lock and touches the
//! owner's state, route-policy authorization, only try-locks it, so it can
//! never wait against an owner that holds its state while lowering a mark.
//!
//! # The first-party mark
//!
//! A record starts unmarked. The configuration gate activates it at most once,
//! when the spawn has acknowledged its module's active generation and reported
//! ready ([`SpawnAuthorityRecord::activate`]). Activation publishes the mark
//! without bumping the epoch and closes no route: a bind captured before it
//! keeps its `first_party: false`, which under-claims and is harmless. After
//! activation the mark can only be lowered ([`SpawnAuthorityRecord::lower`]),
//! which bumps the epoch and closes the spawn's routes. A lowered record is
//! never activated again, and neither is a record lowered before it was
//! activated. The readiness and acknowledgement facts the gate decides from
//! are kept here, on the spawn they describe.
//!
//! # Seams for later work
//!
//! Until program origins are derived from release and placement evidence,
//! every record's origin is [`unproven_origin`], which never qualifies for the
//! mark. Until provider declarations are approved, every record's approved set
//! is [`ApprovedDeclarations::none_approved`], with the digest of the empty set.

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
    /// No origin currently qualifies for approvals. Hash the empty set with
    /// the same algorithm used for requested declarations, never an empty string.
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
pub(crate) fn declarations_digest(entries: &ProviderDeclarations) -> Result<String, String> {
    let mut digests = serde_json::Map::new();
    for (provider, entry) in entries {
        let value = serde_json::json!({"provider": provider, "entry": entry});
        digests.insert(
            provider.clone(),
            serde_json::Value::String(jcs_digest(&value)?),
        );
    }
    jcs_digest(&serde_json::Value::Object(digests))
}

fn jcs_digest(value: &serde_json::Value) -> Result<String, String> {
    let bytes = subc_config::canonical_json(value).map_err(|err| err.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// The digest of one approved entry, sent beside it as `declaration_digest`:
/// the per-provider digest [`declarations_digest`] combines.
pub(crate) fn declaration_digest(
    provider: &str,
    entry: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    jcs_digest(&serde_json::json!({"provider": provider, "entry": entry}))
}

/// The origin of every spawn until program origins are derived from release
/// records and placement evidence. `other` never qualifies for the mark.
pub(crate) fn unproven_origin() -> CallerOrigin {
    CallerOrigin::Other
}

/// Whether this daemon is a development build: one that contains the
/// development placement switch. Only there does a `development` origin
/// qualify for the mark; a release daemon never contains the switch.
const DEVELOPMENT_DAEMON: bool = cfg!(feature = "dev-placement");

/// Where a spawn stands on the first-party mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkStage {
    /// Not yet activated; activation may still happen.
    Unmarked,
    /// Activated and not lowered: the spawn carries the mark.
    Marked,
    /// Lowered, or ruled out before activation. Final.
    Lowered,
}

/// Why a spawn does or does not carry the mark, for reports and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkStatus {
    /// Activated and not lowered.
    Marked,
    /// The mark was lowered, or ruled out for good before activation (for
    /// example by an unstamp).
    Lowered,
    /// The spawn can never be marked: it was admitted without a stamp, its
    /// module is not gated, or its program's origin does not qualify.
    NotEligible,
    /// Waiting for the spawn to acknowledge its module's active generation.
    AwaitingAcknowledgement,
    /// The spawn refused or let time out the acknowledgement it was asked
    /// for, so it stays unmarked.
    AcknowledgementRefused,
    /// Waiting for the spawn to report ready. A module that holds back
    /// readiness until a first-party grant arrives stays here for good.
    AwaitingReadiness,
}

#[derive(Debug)]
struct AuthorityState {
    origin: CallerOrigin,
    stage: MarkStage,
    status: MarkStatus,
    development_daemon: bool,
    /// The spawn reported ready (a healthy probe) at least once.
    ready: bool,
    /// The last generation this spawn acknowledged while it was, or became,
    /// its module's active generation.
    acknowledged_active: Option<u64>,
    /// The active generation the gate already asked this spawn to
    /// acknowledge, so it asks once per generation.
    acknowledgement_requested: Option<u64>,
    /// The readiness warning has been logged for this spawn.
    readiness_reported: bool,
    epoch: u64,
    alive: bool,
}

/// What [`SpawnAuthorityRecord::change_authority`] and
/// [`SpawnAuthorityRecord::lower`] hand to the publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthorityChange {
    /// The spawn's new epoch.
    pub(crate) epoch: u64,
    /// Whether the spawn's child is still running.
    pub(crate) alive: bool,
    /// Whether the spawn still carries the mark after the change.
    pub(crate) first_party: bool,
}

/// One supervised spawn's authority record. See the module docs.
#[derive(Debug)]
pub(crate) struct SpawnAuthorityRecord {
    incarnation: String,
    spawn: u64,
    module_id: String,
    // Approvals are fixed empty until origins exist. Keep the record's one
    // value immutable so forwarding can project it without taking state under
    // its own lock. Mutable approvals will need publication with the epoch.
    approved: ApprovedDeclarations,
    // Tests stand in for approvals that release records and development
    // placement records will supply once origins are derived. Set once by a
    // test, before the reads it should affect.
    #[cfg(test)]
    approved_for_test: std::sync::OnceLock<ApprovedDeclarations>,
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

    /// The `caller` key for a `route.bind` to `target_module`. Only an `own`
    /// caller can carry the mark. A marked caller also carries its approved
    /// entry for the target's module, with that entry's digest; no declaration
    /// is sent while the mark is false. `None` only when an identity integer
    /// cannot be represented by every wire reader.
    pub(crate) fn attestation(&self, target_module: &str) -> Option<CallerAttestation> {
        let first_party = self.authority.first_party && self.claim == CallerClaim::Own;
        let attestation = CallerAttestation::new(
            self.authority.incarnation.clone(),
            self.authority.key.spawn,
            self.authority.epoch,
            self.claim.clone(),
            first_party,
            self.authority.origin,
            self.authority.approved.digest.clone(),
        )
        .ok()?;
        let declaration = first_party
            .then(|| self.authority.approved.entries.get(target_module))
            .flatten()
            .and_then(|entry| {
                declaration_digest(target_module, entry)
                    .ok()
                    .map(|digest| (entry.clone(), digest))
            });
        Some(match declaration {
            Some((entry, digest)) => attestation.with_declaration(entry, digest),
            None => attestation,
        })
    }
}

impl SpawnAuthorityRecord {
    /// A live, unmarked record for a spawn that is about to be launched.
    pub(crate) fn new(incarnation: impl Into<String>, spawn: u64, module_id: &str) -> Arc<Self> {
        Arc::new(Self {
            incarnation: incarnation.into(),
            spawn,
            module_id: module_id.to_owned(),
            approved: ApprovedDeclarations::none_approved(),
            #[cfg(test)]
            approved_for_test: std::sync::OnceLock::new(),
            state: Mutex::new(AuthorityState {
                origin: unproven_origin(),
                stage: MarkStage::Unmarked,
                status: MarkStatus::AwaitingAcknowledgement,
                development_daemon: DEVELOPMENT_DAEMON,
                ready: false,
                acknowledged_active: None,
                acknowledgement_requested: None,
                readiness_reported: false,
                epoch: INITIAL_AUTHORITY_EPOCH,
                alive: true,
            }),
        })
    }

    /// Give this record the origin that origin evidence would derive, so
    /// tests of the mark need not depend on that derivation.
    #[cfg(test)]
    pub(crate) fn set_origin_for_test(&self, origin: CallerOrigin) {
        self.lock().origin = origin;
    }

    /// Make this record behave as on a development (`true`) or a release
    /// (`false`) daemon, whichever build the tests run in.
    #[cfg(test)]
    pub(crate) fn set_development_daemon_for_test(&self, development: bool) {
        self.lock().development_daemon = development;
    }

    /// Give this record an approved set before anything reads it.
    #[cfg(test)]
    pub(crate) fn set_approved_for_test(&self, entries: ProviderDeclarations) {
        let digest = declarations_digest(&entries).expect("canonical test declarations");
        self.approved_for_test
            .set(ApprovedDeclarations { entries, digest })
            .expect("a test sets approvals once");
    }

    pub(crate) fn spawn(&self) -> u64 {
        self.spawn
    }

    pub(crate) fn module_id(&self) -> &str {
        &self.module_id
    }

    pub(crate) fn approved(&self) -> &ApprovedDeclarations {
        #[cfg(test)]
        if let Some(approved) = self.approved_for_test.get() {
            return approved;
        }
        &self.approved
    }

    pub(crate) fn key(&self) -> SpawnKey {
        SpawnKey {
            module_id: self.module_id.clone(),
            spawn: self.spawn,
        }
    }

    fn lock(&self) -> MutexGuard<'_, AuthorityState> {
        crate::forwarding::assert_forwarding_unlocked("a spawn authority record's state");
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
            first_party: state.stage == MarkStage::Marked,
            origin: state.origin,
            approved: self.approved().clone(),
        })
    }

    pub(crate) fn alive(&self) -> bool {
        self.lock().alive
    }

    /// Whether the spawn carries the mark now.
    #[cfg(test)]
    pub(crate) fn first_party(&self) -> bool {
        let state = self.lock();
        state.alive && state.stage == MarkStage::Marked
    }

    /// Why the spawn does or does not carry the mark.
    pub(crate) fn mark_status(&self) -> MarkStatus {
        self.lock().status
    }

    /// The spawn reported ready.
    pub(crate) fn note_ready(&self) {
        self.lock().ready = true;
    }

    /// The spawn acknowledged `generation`, which is now its module's active
    /// generation.
    pub(crate) fn note_active_acknowledgement(&self, generation: u64) {
        self.lock().acknowledged_active = Some(generation);
    }

    /// Record that the gate asks this spawn to acknowledge the active
    /// `generation`. `false` when it was already asked, or already
    /// acknowledged it, so the gate asks at most once per generation.
    pub(crate) fn request_acknowledgement(&self, generation: u64) -> bool {
        let mut state = self.lock();
        if !state.alive
            || state.stage != MarkStage::Unmarked
            || state.acknowledged_active == Some(generation)
            || state.acknowledgement_requested == Some(generation)
        {
            return false;
        }
        state.acknowledgement_requested = Some(generation);
        true
    }

    /// The facts the gate decides activation from, read in one step.
    pub(crate) fn mark_facts(&self) -> MarkFacts {
        let state = self.lock();
        MarkFacts {
            unmarked: state.alive && state.stage == MarkStage::Unmarked,
            marked: state.alive && state.stage == MarkStage::Marked,
            origin_qualifies: match state.origin {
                CallerOrigin::Release => true,
                CallerOrigin::Development => state.development_daemon,
                _ => false,
            },
            ready: state.ready,
            acknowledged_active: state.acknowledged_active,
        }
    }

    /// Record why an unmarked spawn is not yet marked. Returns `true` the first
    /// time the spawn is found waiting for readiness, so that is logged once.
    pub(crate) fn set_unmarked_status(&self, status: MarkStatus) -> bool {
        let mut state = self.lock();
        if state.stage != MarkStage::Unmarked {
            return false;
        }
        state.status = status;
        let first_report = status == MarkStatus::AwaitingReadiness && !state.readiness_reported;
        if first_report {
            state.readiness_reported = true;
        }
        first_report
    }

    /// Activate the mark: the gate found the spawn running its module's
    /// active generation, ready, and qualified. `publish` publishes the mark
    /// to the forwarding table while this record's state is held; the epoch is
    /// not bumped and no route is closed. Happens at most once, and never for
    /// a dead or lowered record. `true` when the mark was raised.
    pub(crate) fn activate<E>(&self, publish: impl FnOnce() -> Result<(), E>) -> Result<bool, E> {
        let mut state = self.lock();
        if !state.alive || state.stage != MarkStage::Unmarked {
            return Ok(false);
        }
        publish()?;
        state.stage = MarkStage::Marked;
        state.status = MarkStatus::Marked;
        Ok(true)
    }

    /// Lower the mark for good. A marked spawn's epoch is bumped and `publish`
    /// runs with the change while this record's state is held, to publish the
    /// epoch and select the routes to close. An unmarked spawn is only ruled
    /// out of a later activation: nothing it attested changes, so nothing is
    /// published and `None` is returned.
    pub(crate) fn lower<R>(&self, publish: impl FnOnce(AuthorityChange) -> R) -> Option<R> {
        let mut state = self.lock();
        let was_marked = state.stage == MarkStage::Marked;
        state.stage = MarkStage::Lowered;
        state.status = MarkStatus::Lowered;
        if !was_marked {
            return None;
        }
        state.epoch = state.epoch.saturating_add(1);
        Some(publish(AuthorityChange {
            epoch: state.epoch,
            alive: state.alive,
            first_party: false,
        }))
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
    /// change, while this record's state is held. `publish` takes the
    /// forwarding write lock to publish the epoch and the mark and select the
    /// routes to close, so they happen in one critical section with the
    /// authority state taken first.
    pub(crate) fn change_authority<R>(&self, publish: impl FnOnce(AuthorityChange) -> R) -> R {
        let mut state = self.lock();
        state.epoch = state.epoch.saturating_add(1);
        publish(AuthorityChange {
            epoch: state.epoch,
            alive: state.alive,
            first_party: state.alive && state.stage == MarkStage::Marked,
        })
    }
}

/// A spawn's mark facts, read in one step for the gate's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MarkFacts {
    /// Alive and not yet activated or lowered.
    pub(crate) unmarked: bool,
    /// Alive and carrying the mark.
    pub(crate) marked: bool,
    /// The program's origin qualifies on this daemon.
    pub(crate) origin_qualifies: bool,
    /// The spawn reported ready.
    pub(crate) ready: bool,
    /// The last active generation the spawn acknowledged.
    pub(crate) acknowledged_active: Option<u64>,
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
            .attestation("module")
            .unwrap();
            assert!(!caller.first_party, "{claim:?}");
            assert_eq!(caller.claim, claim);
            assert!(caller.declaration.is_none());
            assert!(caller.declaration_digest.is_none());
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "while this thread holds the forwarding lock")]
    fn spawn_authority_record_state_is_never_taken_under_the_forwarding_lock() {
        let forwarding = crate::forwarding::ForwardingTable::default();
        let record = SpawnAuthorityRecord::new("incarnation", 1, "module");
        forwarding.with_forwarding_write_lock_for_test(|| {
            let _ = record.capture();
        });
    }

    fn released(spawn: u64) -> Arc<SpawnAuthorityRecord> {
        let record = SpawnAuthorityRecord::new("incarnation", spawn, "module");
        record.set_origin_for_test(CallerOrigin::Release);
        record
    }

    #[test]
    fn spawn_authority_mark_activation_keeps_the_epoch_and_lowering_bumps_it() {
        let record = released(1);
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(true));
        assert!(record.first_party());
        assert_eq!(record.mark_status(), MarkStatus::Marked);
        assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH);
        assert!(record.capture().unwrap().first_party);
        // Activation happens once.
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(false));

        let change = record
            .lower(|change| change)
            .expect("a marked spawn publishes");
        assert_eq!(
            change,
            AuthorityChange {
                epoch: INITIAL_AUTHORITY_EPOCH + 1,
                alive: true,
                first_party: false,
            }
        );
        assert!(!record.first_party());
        assert_eq!(record.mark_status(), MarkStatus::Lowered);
        // After lowering the mark is never raised again.
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(false));
        assert!(record.lower(|change| change).is_none());
        assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH + 1);
    }

    #[test]
    fn spawn_authority_mark_failed_publication_leaves_the_record_unmarked() {
        let record = released(1);
        assert_eq!(record.activate(|| Err("forwarding")), Err("forwarding"));
        assert!(!record.first_party());
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(true));
    }

    #[test]
    fn spawn_authority_mark_lowering_an_unmarked_record_rules_out_activation() {
        let record = released(1);
        assert!(record.lower(|change| change).is_none());
        assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH);
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(false));
        assert!(!record.first_party());
    }

    #[test]
    fn spawn_authority_mark_dead_record_is_never_activated() {
        let record = released(1);
        drop(record.begin_retirement());
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(false));
        assert!(!record.mark_facts().unmarked);
    }

    #[test]
    fn spawn_authority_mark_development_origin_qualifies_only_on_a_development_daemon() {
        let record = SpawnAuthorityRecord::new("incarnation", 1, "module");
        assert!(!record.mark_facts().origin_qualifies, "unproven origin");
        record.set_origin_for_test(CallerOrigin::Development);
        record.set_development_daemon_for_test(false);
        assert!(!record.mark_facts().origin_qualifies);
        record.set_development_daemon_for_test(true);
        assert!(record.mark_facts().origin_qualifies);
        assert!(
            !SpawnAuthorityRecord::new("i", 2, "m")
                .mark_facts()
                .origin_qualifies
        );
    }

    #[test]
    fn spawn_authority_mark_marked_own_caller_carries_its_approved_entry() {
        let record = released(3);
        let entry: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({"scopes": ["read"]})).unwrap();
        record.set_approved_for_test(ProviderDeclarations::from([(
            "vault".to_owned(),
            entry.clone(),
        )]));
        assert_eq!(record.activate(|| Ok::<_, ()>(())), Ok(true));
        let capture = |claim| CallerCapture {
            authority: record.capture().unwrap(),
            claim,
        };
        let own = capture(CallerClaim::Own).attestation("vault").unwrap();
        assert!(own.first_party);
        assert_eq!(own.declaration, Some(entry.clone()));
        assert_eq!(
            own.declaration_digest,
            Some(declaration_digest("vault", &entry).unwrap())
        );
        assert_eq!(own.approved_declarations_digest, record.approved().digest);
        // No entry for another provider; no mark or entry for other claims.
        let other = capture(CallerClaim::Own).attestation("plexus").unwrap();
        assert!(other.first_party);
        assert!(other.declaration.is_none() && other.declaration_digest.is_none());
        for claim in [
            CallerClaim::Brokered,
            CallerClaim::Relay,
            CallerClaim::Legacy,
        ] {
            let caller = capture(claim.clone()).attestation("vault").unwrap();
            assert!(!caller.first_party, "{claim:?}");
            assert!(caller.declaration.is_none(), "{claim:?}");
        }
    }
}
