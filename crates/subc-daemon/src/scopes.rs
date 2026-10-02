//! The daemon's scope table.
//!
//! A scope is an owned identity record for a session, keyed `(owner, ref)`. The
//! owner is the module whose own registered connection synced it; the daemon
//! never takes the owner from a request body. Everything here is in memory and
//! starts empty in every daemon incarnation: owners re-sync after a restart and
//! readers hold until they do (`docs/designs/daemon-scopes.md`, sections 2, 3,
//! 5 and 6).
//!
//! The table is pure state. The control handler decides who the syncing owner
//! is and whether its connection belongs to the owner's current launch, and
//! passes both in, so every rule below can be exercised without a socket.
//!
//! Route admission ([`ScopeTable::admit`]) reads the table and returns the stamp
//! for the bind and the tag it was taken at. A sync reports each scope whose
//! `(scope_epoch, version)` it changed ([`ScopeTagChange`]), with which live
//! routes the change closes ([`ScopeDrain`]), so the caller publishes the tags
//! into the forwarding table and closes those routes in the same step: a bind
//! commit compares its captured tag against the published one and must never
//! take the scope lock.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use subc_protocol::{
    error_codes,
    scope::{
        ParentState, ScopeEnded, ScopeParent, ScopeRecord, ScopeRecordOutcome, ScopeRecordResult,
        ScopeSelector, ScopeStamp, ScopeStatus, MAX_CARRIER_TARGETS, MAX_LIVE_SCOPES_PER_OWNER,
        MAX_SCOPE_ATTRIBUTE_BYTES, MAX_SCOPE_TOMBSTONES_PER_OWNER,
    },
    Principal, RouteCloseReason,
};

use crate::registry::ConnectionId;

/// The code a malformed sync body is refused with, the same one the control
/// plane uses for any body that does not parse.
const INVALID_CONTROL_BODY: &str = "invalid_control_body";

/// The launch nonce each module connection presented in its HELLO, kept so the
/// scope table can tell whether a connection belongs to its module's current
/// launch. The registry keeps no nonce, and the supervisor's record says only
/// which nonce is current, not which connection holds it.
#[derive(Default)]
pub(crate) struct HelloLaunchNonces {
    by_connection: HashMap<ConnectionId, String>,
}

// Hand-written so no launch nonce is ever printed. Each nonce is the credential
// that attributes a connection to a supervised module, and a derived Debug
// would write every module's nonce into any log line or panic message that
// formats the handler or this map. Same reasoning as ConsumerIdentity's Debug.
impl std::fmt::Debug for HelloLaunchNonces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelloLaunchNonces")
            .field("connections", &self.by_connection.len())
            .finish()
    }
}

impl HelloLaunchNonces {
    /// Record what `connection_id` presented, replacing any earlier HELLO on the
    /// same connection. `None` forgets it: a HELLO without a nonce proves no
    /// launch.
    pub(crate) fn record(&mut self, connection_id: ConnectionId, nonce: Option<&str>) {
        match nonce {
            Some(nonce) if !nonce.is_empty() => {
                self.by_connection.insert(connection_id, nonce.to_string());
            }
            _ => {
                self.by_connection.remove(&connection_id);
            }
        }
    }

    pub(crate) fn forget(&mut self, connection_id: ConnectionId) {
        self.by_connection.remove(&connection_id);
    }

    /// Whether `connection_id` presented `current`, the module's recorded
    /// spawn nonce. No recorded nonce (a module the supervisor did not spawn)
    /// is never current.
    pub(crate) fn presented(&self, connection_id: ConnectionId, current: Option<&str>) -> bool {
        match (self.by_connection.get(&connection_id), current) {
            (Some(presented), Some(current)) => {
                constant_time_eq(presented.as_bytes(), current.as_bytes())
            }
            _ => false,
        }
    }
}

/// Byte comparison whose time does not depend on where the inputs differ, so
/// comparing a presented nonce leaks nothing about how much of it matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// What a bind captures at admission and compares at commit: the session the
/// scope names and the daemon's counter of its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeTag {
    pub(crate) scope_epoch: u64,
    pub(crate) version: u64,
}

/// One scope whose tag a sync changed. `after: None` means the scope ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeTagChange {
    pub(crate) owner: String,
    pub(crate) scope_ref: String,
    pub(crate) before: Option<ScopeTag>,
    pub(crate) after: Option<ScopeTag>,
    /// Which live routes under the scope's `before` epoch the change closes.
    pub(crate) drain: ScopeDrain,
}

/// Which live routes a change to one scope closes (the drain table in
/// `docs/designs/daemon-scopes.md`). Only changes that take authority away
/// close anything: the scope ending, a carrier or one of its target modules
/// removed, `delegates` turned off or `agent_id` changed, or the parent
/// ending. Any other change leaves routes up even though their stamp now
/// carries an older version, because that stamp grants nothing the current
/// record no longer grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeDrain {
    /// Nothing: a carrier or target added, `child_owners` changed, a parent
    /// link going from pending to linked, or any other change that narrows
    /// no one's authority.
    Nothing,
    /// Every route under the scope, with this reason.
    All(RouteCloseReason),
    /// Only routes opened by these carriers: every such route when the
    /// allowance is `None` (the carrier entry was removed), or those whose
    /// target module is outside the allowance (targets were removed).
    Carriers(Vec<(Principal, Option<BTreeSet<String>>)>),
}

impl ScopeDrain {
    /// Rank for combining two drains of one scope in the same sync: closing
    /// every route beats closing some carriers' routes, and among reasons
    /// that close every route the most final is reported (the scope ended,
    /// then its parent ended, then its delegation changed), so a client is
    /// told the reason that also explains why reopening will not work.
    fn rank(&self) -> u8 {
        match self {
            Self::Nothing => 0,
            Self::Carriers(_) => 1,
            Self::All(RouteCloseReason::ScopeDelegationChanged) => 2,
            Self::All(RouteCloseReason::ScopeParentEnded) => 3,
            Self::All(_) => 4,
        }
    }

    fn widen(self, other: ScopeDrain) -> ScopeDrain {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// The modules one principal may open to under a record: `None` if it may not
/// open at all, `Some(None)` for any module, `Some(Some(set))` for those only.
/// The owner is not a carrier and is not answered here.
fn carrier_allowance(
    record: &ScopeRecord,
    principal: &Principal,
) -> Option<Option<BTreeSet<String>>> {
    let mut allowance: Option<Option<BTreeSet<String>>> = None;
    for carrier in record.carriers.iter().filter(|c| &c.principal == principal) {
        allowance = Some(match (allowance, &carrier.targets) {
            (Some(None), _) | (_, None) => None,
            (Some(Some(mut set)), Some(targets)) => {
                set.extend(targets.iter().cloned());
                Some(set)
            }
            (None, Some(targets)) => Some(targets.iter().cloned().collect()),
        });
    }
    allowance
}

/// Which routes of the same scope a content change at one epoch closes.
fn drain_for_change(before: &LiveScope, after: &LiveScope) -> ScopeDrain {
    let mut drain = ScopeDrain::Nothing;
    let mut narrowed = Vec::new();
    let mut principals: Vec<&Principal> = Vec::new();
    for carrier in &before.record.carriers {
        if !principals.contains(&&carrier.principal) {
            principals.push(&carrier.principal);
        }
    }
    for principal in principals {
        let was = carrier_allowance(&before.record, principal);
        let now = carrier_allowance(&after.record, principal);
        match (was, now) {
            (Some(_), None) => narrowed.push((principal.clone(), None)),
            (Some(None), Some(Some(set))) => narrowed.push((principal.clone(), Some(set))),
            (Some(Some(old)), Some(Some(set))) if !old.is_subset(&set) => {
                narrowed.push((principal.clone(), Some(set)))
            }
            _ => {}
        }
    }
    if !narrowed.is_empty() {
        drain = ScopeDrain::Carriers(narrowed);
    }
    let before_attributes = &before.record.attributes;
    let after_attributes = &after.record.attributes;
    if (before_attributes.delegates && !after_attributes.delegates)
        || before_attributes.agent_id != after_attributes.agent_id
    {
        drain = drain.widen(ScopeDrain::All(RouteCloseReason::ScopeDelegationChanged));
    }
    if after.parent_state == Some(ParentState::Ended)
        && before.parent_state != Some(ParentState::Ended)
    {
        drain = drain.widen(ScopeDrain::All(RouteCloseReason::ScopeParentEnded));
    }
    drain
}

/// The scope a pending bind or a live route was admitted under, as the
/// forwarding table keeps it: enough to compare against the published tag at
/// commit and to find the route when the scope changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundScope {
    pub(crate) owner: String,
    pub(crate) scope_ref: String,
    pub(crate) tag: ScopeTag,
}

/// An admitted scoped open: what the bind is stamped with, and the tag the
/// commit compares against the published one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeAdmission {
    pub(crate) owner: String,
    pub(crate) stamp: ScopeStamp,
    pub(crate) tag: ScopeTag,
}

/// A refused scoped open: a code from `error_codes` and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeAdmissionRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

/// An accepted sync: the reply's contents plus the tag changes to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncApplied {
    pub(crate) results: Vec<ScopeRecordResult>,
    pub(crate) ended: Vec<ScopeEnded>,
    pub(crate) tag_changes: Vec<ScopeTagChange>,
}

/// A refused sync. Nothing in the table changed, sync authority included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

/// The table's half of a `scope.describe` answer. The caller adds what the
/// table cannot know: the daemon incarnation and whether the owner is in the
/// supervisor's roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeDescription {
    pub(crate) status: ScopeStatus,
    pub(crate) scope_epoch: Option<u64>,
    pub(crate) owner_synced: bool,
    pub(crate) stamp: Option<ScopeStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveScope {
    record: ScopeRecord,
    version: u64,
    /// `None` exactly when the record has no parent.
    parent_state: Option<ParentState>,
}

impl LiveScope {
    fn tag(&self) -> ScopeTag {
        ScopeTag {
            scope_epoch: self.record.scope_epoch,
            version: self.version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyncAuthority {
    connection_id: ConnectionId,
    last_generation: u64,
}

/// The `(ref, scope_epoch)` pairs one owner ended in this incarnation, oldest
/// first, bounded by [`MAX_SCOPE_TOMBSTONES_PER_OWNER`]. Evicting the oldest
/// never refuses anything: a reader that finds nothing reads `not_live`, which
/// after the owner has synced means the same as ended.
#[derive(Debug, Default)]
struct Tombstones {
    order: VecDeque<(String, u64)>,
    by_ref: HashMap<String, BTreeSet<u64>>,
}

impl Tombstones {
    fn insert(&mut self, scope_ref: &str, scope_epoch: u64) {
        if !self
            .by_ref
            .entry(scope_ref.to_string())
            .or_default()
            .insert(scope_epoch)
        {
            return;
        }
        self.order.push_back((scope_ref.to_string(), scope_epoch));
        while self.order.len() > MAX_SCOPE_TOMBSTONES_PER_OWNER {
            let Some((evicted_ref, evicted_epoch)) = self.order.pop_front() else {
                break;
            };
            if let Some(epochs) = self.by_ref.get_mut(&evicted_ref) {
                epochs.remove(&evicted_epoch);
                if epochs.is_empty() {
                    self.by_ref.remove(&evicted_ref);
                }
            }
        }
    }

    fn contains(&self, scope_ref: &str, scope_epoch: u64) -> bool {
        self.by_ref
            .get(scope_ref)
            .is_some_and(|epochs| epochs.contains(&scope_epoch))
    }

    fn latest(&self, scope_ref: &str) -> Option<u64> {
        self.by_ref
            .get(scope_ref)
            .and_then(|epochs| epochs.last().copied())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

#[derive(Debug, Default)]
struct OwnerScopes {
    authority: Option<SyncAuthority>,
    /// Whether this owner has had a sync accepted in this incarnation. Readers
    /// need it to tell "not re-synced yet" from "gone", and parent links need
    /// it to tell "unverified" from "refused".
    synced: bool,
    live: BTreeMap<String, LiveScope>,
    tombstones: Tombstones,
}

/// All scopes, per owner, for one daemon incarnation.
#[derive(Debug)]
pub(crate) struct ScopeTable {
    /// Module ids whose scopes may carry `agent_id` and `delegates`. Fixed for
    /// the daemon's life: the config key is restart-required, so no live route
    /// can hold an `owner_authorized` stamp its owner has since lost.
    authority_owners: BTreeSet<String>,
    owners: HashMap<String, OwnerScopes>,
    /// Source of every `version`. One counter for the whole table, so a version
    /// is never reused within an incarnation even across an epoch change.
    last_version: u64,
    #[cfg(test)]
    link_lookups: std::sync::atomic::AtomicUsize,
}

fn reserved_module_id(principal: &Principal) -> Option<&str> {
    match principal {
        Principal::Reserved { module_id } => Some(module_id),
        _ => None,
    }
}

fn reserved(module_id: &str) -> Principal {
    Principal::Reserved {
        module_id: module_id.to_string(),
    }
}

/// A per-record refusal: the code and a message naming what was wrong.
type RecordRefusal = (&'static str, String);

/// The owner whose sync is in progress, and its set as it will stand.
type Overlay<'a> = Option<(&'a str, &'a BTreeMap<String, ScopeRecord>)>;

impl ScopeTable {
    pub(crate) fn new(authority_owners: impl IntoIterator<Item = String>) -> Self {
        Self {
            authority_owners: authority_owners.into_iter().collect(),
            owners: HashMap::new(),
            last_version: 0,
            #[cfg(test)]
            link_lookups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn owner_authorized(&self, owner: &str) -> bool {
        self.authority_owners.contains(owner)
    }

    fn next_version(&mut self) -> u64 {
        self.last_version += 1;
        self.last_version
    }

    /// Forget sync authority held by a connection that has gone away, so the
    /// owner's next connection to sync can take it.
    pub(crate) fn release_connection(&mut self, connection_id: ConnectionId) {
        for state in self.owners.values_mut() {
            if state
                .authority
                .is_some_and(|authority| authority.connection_id == connection_id)
            {
                state.authority = None;
            }
        }
    }

    /// Apply `owner`'s full scope set.
    ///
    /// `is_current_launch` answers, for a connection of this owner, whether it
    /// belongs to the owner's current launch: the one whose launch nonce the
    /// supervisor records as the module's spawn nonce. A blue/green candidate
    /// before cutover, an incumbent after it, and a module the supervisor did
    /// not spawn are all not current.
    pub(crate) fn sync(
        &mut self,
        owner: &str,
        connection_id: ConnectionId,
        is_current_launch: impl Fn(ConnectionId) -> bool,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> Result<SyncApplied, SyncRefusal> {
        // Taken out of the map for the duration so the other owners' scopes can
        // be read while this owner's are rebuilt. Every path puts it back.
        let mut state = self.owners.remove(owner).unwrap_or_default();
        let outcome = self.sync_owner(
            owner,
            &mut state,
            connection_id,
            &is_current_launch,
            generation,
            scopes,
        );
        self.owners.insert(owner.to_string(), state);
        let mut applied = outcome?;
        self.refresh_links_to(owner, &mut applied.tag_changes);
        // The refresh can change parent states and versions, so the reply is
        // read from the table after it rather than before.
        let state = &self.owners[owner];
        let moved: HashSet<_> = applied
            .tag_changes
            .iter()
            .filter(|change| change.owner == owner)
            .map(|change| change.scope_ref.as_str())
            .collect();
        for result in &mut applied.results {
            let live = state.live.get(&result.scope_ref);
            result.version = live.map(|scope| scope.version);
            result.parent_state = live.and_then(|scope| scope.parent_state);
            // A record re-sent unchanged can still change here, when the same
            // sync ended the parent its link names.
            let tag_moved = moved.contains(result.scope_ref.as_str());
            if result.outcome == ScopeRecordOutcome::Unchanged && tag_moved {
                result.outcome = ScopeRecordOutcome::Updated;
            }
        }
        Ok(applied)
    }

    fn sync_owner(
        &mut self,
        owner: &str,
        state: &mut OwnerScopes,
        connection_id: ConnectionId,
        is_current_launch: &impl Fn(ConnectionId) -> bool,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> Result<SyncApplied, SyncRefusal> {
        let taking_authority = Self::check_authority(state, connection_id, is_current_launch)?;
        if let (false, Some(authority)) = (taking_authority, state.authority) {
            if generation <= authority.last_generation {
                return Err(SyncRefusal {
                    code: error_codes::SCOPE_SYNC_STALE,
                    message: format!(
                        "generation {generation} is not larger than the last accepted generation {}",
                        authority.last_generation
                    ),
                });
            }
        }
        Self::check_sync_bounds(&scopes)?;

        let owner_authorized = self.owner_authorized(owner);
        let mut refusals: HashMap<usize, RecordRefusal> = HashMap::new();
        // The set as it will stand after this sync. A refused record keeps its
        // previous state, so its held record (if any) stays in the set.
        let mut next: BTreeMap<String, ScopeRecord> = BTreeMap::new();
        for (index, record) in scopes.iter().enumerate() {
            let held = state.live.get(&record.scope_ref);
            match Self::record_refusal(record, held, &state.tombstones, owner_authorized) {
                Some(refusal) => {
                    refusals.insert(index, refusal);
                    if let Some(held) = held {
                        next.insert(record.scope_ref.clone(), held.record.clone());
                    }
                }
                None => {
                    next.insert(record.scope_ref.clone(), record.clone());
                }
            }
        }

        // Refusing a parent only invalidates its direct children. Settle these
        // dependencies with a queue before walking ancestry for cycles, so a
        // leaf-first chain with a refused root does not repeatedly walk every
        // surviving ancestor under the scope write lock.
        let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut new_links = vec![false; scopes.len()];
        for (index, record) in scopes.iter().enumerate() {
            let Some(parent) = record.parent.as_ref() else {
                continue;
            };
            new_links[index] = state.live.get(&record.scope_ref).is_none_or(|held| {
                held.record.scope_epoch != record.scope_epoch
                    || held.record.parent.as_ref() != Some(parent)
            });
            if reserved_module_id(&parent.owner) == Some(owner) {
                children.entry(&parent.scope_ref).or_default().push(index);
            }
        }
        let mut pending: VecDeque<usize> = (0..scopes.len()).collect();
        let mut queued = vec![true; scopes.len()];
        let mut new_link_states: HashMap<String, ParentState> = HashMap::new();
        loop {
            let mut rejected = None;
            while let Some(index) = pending.pop_front() {
                queued[index] = false;
                if refusals.contains_key(&index) || !new_links[index] {
                    continue;
                }
                let record = &scopes[index];
                let parent = record.parent.as_ref().expect("new link has a parent");
                // A link the daemon already accepted is not re-checked when the
                // owner re-sends it unchanged: its state is kept current by the
                // parent's owner's syncs. Re-checking it would refuse the child
                // record on every later sync once its parent ended, although
                // the link was valid when it was made.
                match self.check_new_link(owner, parent, &next) {
                    Ok(link_state) => {
                        new_link_states.insert(record.scope_ref.clone(), link_state);
                    }
                    Err(message) => {
                        rejected = Some((index, message));
                        break;
                    }
                }
            }
            if rejected.is_none() {
                // Memoize acyclic ancestry only for this stable overlay. A
                // refused cyclic link can restore an older record, so any
                // topology change discards the cache before checking again.
                let mut acyclic = HashSet::new();
                for (index, record) in scopes.iter().enumerate() {
                    if new_link_states.get(&record.scope_ref) == Some(&ParentState::Linked)
                        && self.link_closes_cycle_cached(
                            Some((owner, &next)),
                            owner,
                            &record.scope_ref,
                            record.parent.as_ref().unwrap(),
                            &mut acyclic,
                        )
                    {
                        rejected = Some((index, "the parent link would close a cycle".to_string()));
                        break;
                    }
                }
            }
            let Some((index, message)) = rejected else {
                break;
            };
            let record = &scopes[index];
            refusals.insert(index, (error_codes::SCOPE_PARENT_NOT_PERMITTED, message));
            new_link_states.remove(&record.scope_ref);
            match state.live.get(&record.scope_ref) {
                Some(held) => {
                    next.insert(record.scope_ref.clone(), held.record.clone());
                }
                None => {
                    next.remove(&record.scope_ref);
                }
            }
            for &child in children
                .get(record.scope_ref.as_str())
                .into_iter()
                .flatten()
            {
                if !queued[child] {
                    pending.push_back(child);
                    queued[child] = true;
                }
            }
        }

        // Commit. Nothing above touched `state`.
        let old = std::mem::take(&mut state.live);
        let mut ended = Vec::new();
        for (scope_ref, held) in &old {
            let replaced = next
                .get(scope_ref)
                .is_none_or(|record| record.scope_epoch != held.record.scope_epoch);
            if replaced {
                state.tombstones.insert(scope_ref, held.record.scope_epoch);
                ended.push(ScopeEnded {
                    scope_ref: scope_ref.clone(),
                    scope_epoch: held.record.scope_epoch,
                });
            }
        }
        for (scope_ref, record) in next {
            let held = old
                .get(&scope_ref)
                .filter(|held| held.record.scope_epoch == record.scope_epoch);
            let parent_state = match (&record.parent, held) {
                (None, _) => None,
                (Some(_), _) if new_link_states.contains_key(&scope_ref) => {
                    new_link_states.get(&scope_ref).copied()
                }
                (Some(_), Some(held)) => held.parent_state,
                // Unreachable in practice: a new record with a parent always
                // passed through the link check above. Pending grants nothing.
                (Some(_), None) => Some(ParentState::Pending),
            };
            let version = match held {
                Some(held) if held.record == record && held.parent_state == parent_state => {
                    held.version
                }
                _ => self.next_version(),
            };
            state.live.insert(
                scope_ref,
                LiveScope {
                    record,
                    version,
                    parent_state,
                },
            );
        }
        state.synced = true;
        state.authority = Some(SyncAuthority {
            connection_id,
            last_generation: generation,
        });

        let mut tag_changes = Vec::new();
        let refs: BTreeSet<&String> = old.keys().chain(state.live.keys()).collect();
        for scope_ref in refs {
            let old_scope = old.get(scope_ref);
            let new_scope = state.live.get(scope_ref);
            let before = old_scope.map(LiveScope::tag);
            let after = new_scope.map(LiveScope::tag);
            if before != after {
                let drain = match (old_scope, new_scope) {
                    (Some(old_scope), Some(new_scope))
                        if old_scope.record.scope_epoch == new_scope.record.scope_epoch =>
                    {
                        drain_for_change(old_scope, new_scope)
                    }
                    // Removed, or replaced by a higher epoch: a new session never
                    // inherits a live route of the old one.
                    (Some(_), _) => ScopeDrain::All(RouteCloseReason::ScopeEnded),
                    (None, _) => ScopeDrain::Nothing,
                };
                tag_changes.push(ScopeTagChange {
                    owner: owner.to_string(),
                    scope_ref: scope_ref.clone(),
                    before,
                    after,
                    drain,
                });
            }
        }

        let results = scopes
            .into_iter()
            .enumerate()
            .map(|(index, record)| {
                let (outcome, code, message) = match refusals.remove(&index) {
                    Some((code, message)) => (
                        ScopeRecordOutcome::Refused,
                        Some(code.to_string()),
                        Some(message),
                    ),
                    None => {
                        let held = old.get(&record.scope_ref);
                        let live = state.live.get(&record.scope_ref);
                        let outcome = match (held, live) {
                            (None, _) => ScopeRecordOutcome::Created,
                            (Some(held), _) if held.record.scope_epoch != record.scope_epoch => {
                                ScopeRecordOutcome::Replaced
                            }
                            (Some(held), Some(live)) if held.version == live.version => {
                                ScopeRecordOutcome::Unchanged
                            }
                            _ => ScopeRecordOutcome::Updated,
                        };
                        (outcome, None, None)
                    }
                };
                ScopeRecordResult {
                    scope_ref: record.scope_ref,
                    scope_epoch: record.scope_epoch,
                    outcome,
                    code,
                    message,
                    // Filled in by `sync` once parent links are refreshed.
                    version: None,
                    parent_state: None,
                }
            })
            .collect();

        Ok(SyncApplied {
            results,
            ended,
            tag_changes,
        })
    }

    /// Whether this connection may sync for the owner, and whether doing so
    /// takes authority (which makes this sync a full replace at any generation).
    fn check_authority(
        state: &OwnerScopes,
        connection_id: ConnectionId,
        is_current_launch: &impl Fn(ConnectionId) -> bool,
    ) -> Result<bool, SyncRefusal> {
        let not_authority = |message: &str| SyncRefusal {
            code: error_codes::SCOPE_SYNC_NOT_AUTHORITY,
            message: message.to_string(),
        };
        if !is_current_launch(connection_id) {
            // Covers a swap candidate before cutover (its nonce is the swap's
            // candidate token, not the recorded spawn nonce), the incumbent
            // after cutover, and a module the supervisor never spawned.
            return Err(not_authority(
                "this connection is not the owner's current supervised launch",
            ));
        }
        match state.authority {
            Some(authority) if authority.connection_id == connection_id => Ok(false),
            // A newer launch takes authority from an older launch's connection
            // that is still open, so a wedged predecessor cannot lock a
            // restarted owner out. Two connections of the current launch are
            // not ordered by anything, so the second is refused.
            Some(authority) if is_current_launch(authority.connection_id) => Err(not_authority(
                "another connection of the owner's current launch holds sync authority",
            )),
            Some(_) | None => Ok(true),
        }
    }

    /// Bounds that refuse the whole sync, and bodies that are malformed as a
    /// set (a ref twice, or an empty ref).
    fn check_sync_bounds(scopes: &[ScopeRecord]) -> Result<(), SyncRefusal> {
        if scopes.len() > MAX_LIVE_SCOPES_PER_OWNER {
            return Err(SyncRefusal {
                code: error_codes::SCOPE_LIVE_LIMIT_EXCEEDED,
                message: format!(
                    "{} scopes exceed the limit of {MAX_LIVE_SCOPES_PER_OWNER} live scopes per owner",
                    scopes.len()
                ),
            });
        }
        let mut seen = HashSet::new();
        for record in scopes {
            if record.scope_ref.is_empty() {
                return Err(SyncRefusal {
                    code: INVALID_CONTROL_BODY,
                    message: "a scope ref must not be empty".to_string(),
                });
            }
            if !seen.insert(record.scope_ref.as_str()) {
                return Err(SyncRefusal {
                    code: INVALID_CONTROL_BODY,
                    message: format!("scope ref '{}' appears more than once", record.scope_ref),
                });
            }
            let attribute_bytes = serde_json::to_vec(&record.attributes)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX);
            if attribute_bytes > MAX_SCOPE_ATTRIBUTE_BYTES {
                return Err(SyncRefusal {
                    code: error_codes::SCOPE_ATTRIBUTES_TOO_LARGE,
                    message: format!(
                        "scope '{}' carries {attribute_bytes} bytes of attributes, over the \
                         limit of {MAX_SCOPE_ATTRIBUTE_BYTES}",
                        record.scope_ref
                    ),
                });
            }
        }
        Ok(())
    }

    /// The checks one record fails or passes on its own, before parent links.
    fn record_refusal(
        record: &ScopeRecord,
        held: Option<&LiveScope>,
        tombstones: &Tombstones,
        owner_authorized: bool,
    ) -> Option<RecordRefusal> {
        for carrier in &record.carriers {
            if let Some(targets) = &carrier.targets {
                if targets.is_empty() || targets.len() > MAX_CARRIER_TARGETS {
                    return Some((
                        error_codes::SCOPE_CARRIER_TARGETS_INVALID,
                        format!(
                            "a targeted carrier must list 1 to {MAX_CARRIER_TARGETS} modules, \
                             not {}",
                            targets.len()
                        ),
                    ));
                }
            }
        }
        if !record.attributes.is_empty() && !owner_authorized {
            return Some((
                error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED,
                "agent_id and delegates may be set only by an owner listed in \
                 scope_authority_owners"
                    .to_string(),
            ));
        }
        if record.attributes.delegates && record.attributes.agent_id.is_none() {
            return Some((
                error_codes::SCOPE_DELEGATES_WITHOUT_AGENT,
                "delegates requires an agent_id".to_string(),
            ));
        }
        let epoch = record.scope_epoch;
        if tombstones.contains(&record.scope_ref, epoch) {
            return Some((
                error_codes::SCOPE_EPOCH_ENDED,
                format!("scope_epoch {epoch} of this ref already ended; use a higher epoch"),
            ));
        }
        match held {
            Some(held) if epoch < held.record.scope_epoch => Some((
                error_codes::SCOPE_EPOCH_REGRESSED,
                format!(
                    "scope_epoch {epoch} is lower than the live epoch {}",
                    held.record.scope_epoch
                ),
            )),
            Some(held) if epoch == held.record.scope_epoch && record.kind != held.record.kind => {
                Some((
                    error_codes::SCOPE_KIND_CHANGED,
                    format!(
                        "kind is fixed for scope_epoch {epoch}; use a higher epoch to change it"
                    ),
                ))
            }
            Some(_) => None,
            // Not live, but a later epoch of the ref ended in this incarnation:
            // an older session must not come back after a newer one.
            None => tombstones
                .latest(&record.scope_ref)
                .filter(|latest| epoch < *latest)
                .map(|latest| {
                    (
                        error_codes::SCOPE_EPOCH_REGRESSED,
                        format!("scope_epoch {epoch} is lower than the ended epoch {latest}"),
                    )
                }),
        }
    }

    /// The record `owner/scope_ref` names. With an overlay `(syncing owner,
    /// set being synced)`, that owner's scopes are read from the set and every
    /// other owner's from the table; without one, all come from the table.
    fn lookup<'a>(
        &'a self,
        overlay: Overlay<'a>,
        owner: &str,
        scope_ref: &str,
    ) -> Option<&'a ScopeRecord> {
        #[cfg(test)]
        self.link_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match overlay {
            Some((syncing_owner, next)) if syncing_owner == owner => next.get(scope_ref),
            _ => self
                .owners
                .get(owner)
                .and_then(|state| state.live.get(scope_ref))
                .map(|scope| &scope.record),
        }
    }

    /// Check a parent link a sync is creating or changing.
    ///
    /// Checked against the parent's owner's synced set. If that owner has not
    /// synced in this incarnation (owners re-sync in any order after a daemon
    /// restart) the link is accepted as `pending` and settled when it does;
    /// a child is never refused because its parent's owner is late.
    fn check_new_link(
        &self,
        owner: &str,
        parent: &ScopeParent,
        next: &BTreeMap<String, ScopeRecord>,
    ) -> Result<ParentState, String> {
        let Some(parent_owner) = reserved_module_id(&parent.owner) else {
            return Err("a parent's owner must be a supervised module".to_string());
        };
        let parent_owner_synced = parent_owner == owner
            || self
                .owners
                .get(parent_owner)
                .is_some_and(|state| state.synced);
        if !parent_owner_synced {
            // The parent's owner holds no scopes in this incarnation yet, so
            // no cycle can pass through it.
            return Ok(ParentState::Pending);
        }
        let overlay = Some((owner, next));
        let Some(parent_record) = self.lookup(overlay, parent_owner, &parent.scope_ref) else {
            return Err(format!(
                "parent {parent_owner}/{} is not live",
                parent.scope_ref
            ));
        };
        if parent_record.scope_epoch != parent.scope_epoch {
            return Err(format!(
                "parent {parent_owner}/{} is live at scope_epoch {}, not {}",
                parent.scope_ref, parent_record.scope_epoch, parent.scope_epoch
            ));
        }
        if parent_owner != owner && !parent_record.child_owners.contains(&reserved(owner)) {
            return Err(format!(
                "{owner} is neither the owner of parent {parent_owner}/{} nor in its child_owners",
                parent.scope_ref
            ));
        }
        Ok(ParentState::Linked)
    }

    /// Cache paths known to terminate, sharing the work across siblings and
    /// long chains. References avoid cloning a parent record at every hop.
    fn link_closes_cycle_cached<'a>(
        &'a self,
        overlay: Overlay<'a>,
        child_owner: &str,
        child_ref: &str,
        parent: &'a ScopeParent,
        acyclic: &mut HashSet<(&'a str, &'a str, u64)>,
    ) -> bool {
        let mut visited = HashSet::new();
        let mut link = parent;
        while let Some(owner) = reserved_module_id(&link.owner) {
            let key = (owner, link.scope_ref.as_str(), link.scope_epoch);
            if owner == child_owner && link.scope_ref == child_ref {
                return true;
            }
            if acyclic.contains(&key) {
                break;
            }
            if !visited.insert(key) {
                // An unrelated cycle is not evidence this link closes one.
                return false;
            }
            let Some(record) = self.lookup(overlay, owner, &link.scope_ref) else {
                break;
            };
            if record.scope_epoch != link.scope_epoch {
                break;
            }
            let Some(up) = record.parent.as_ref() else {
                break;
            };
            link = up;
        }
        acyclic.extend(visited);
        false
    }

    /// Whether following parent links up from `parent` reaches
    /// `child_owner/child_ref`. Only links whose named epoch is the live one are
    /// followed, since any other link joins nothing.
    fn link_closes_cycle(
        &self,
        overlay: Overlay<'_>,
        child_owner: &str,
        child_ref: &str,
        parent: &ScopeParent,
    ) -> bool {
        let mut visited: HashSet<(String, String)> = HashSet::new();
        let mut link = parent.clone();
        loop {
            let Some(owner) = reserved_module_id(&link.owner) else {
                return false;
            };
            if owner == child_owner && link.scope_ref == child_ref {
                return true;
            }
            if !visited.insert((owner.to_string(), link.scope_ref.clone())) {
                return false;
            }
            let Some(record) = self.lookup(overlay, owner, &link.scope_ref) else {
                return false;
            };
            if record.scope_epoch != link.scope_epoch {
                return false;
            }
            let Some(up) = record.parent.clone() else {
                return false;
            };
            link = up;
        }
    }

    /// Settle every parent link that points into `parent_owner`'s scopes, after
    /// that owner's sync. `pending` becomes `linked` or `ended`; `linked`
    /// becomes `ended` when the parent is gone or at another epoch; `ended`
    /// stays ended, because a link pins one session of the parent and a new
    /// session under the same ref never adopts the old one's children.
    fn refresh_links_to(&mut self, parent_owner: &str, tag_changes: &mut Vec<ScopeTagChange>) {
        let parent_principal = reserved(parent_owner);
        let mut updates: Vec<(String, String, ParentState)> = Vec::new();
        for (child_owner, state) in &self.owners {
            for (child_ref, scope) in &state.live {
                let Some(parent) = scope.record.parent.as_ref() else {
                    continue;
                };
                if parent.owner != parent_principal {
                    continue;
                }
                let current = scope.parent_state;
                let parent_record = self.owners.get(parent_owner).and_then(|parent_state| {
                    parent_state
                        .live
                        .get(&parent.scope_ref)
                        .filter(|parent_scope| {
                            parent_scope.record.scope_epoch == parent.scope_epoch
                        })
                        .map(|parent_scope| &parent_scope.record)
                });
                let settled = match (current, parent_record) {
                    (Some(ParentState::Ended), _) | (_, None) => ParentState::Ended,
                    (Some(ParentState::Linked), Some(_)) => ParentState::Linked,
                    (_, Some(parent_record)) => {
                        let permitted = child_owner == parent_owner
                            || parent_record.child_owners.contains(&reserved(child_owner));
                        if permitted
                            && !self.link_closes_cycle(None, child_owner, child_ref, parent)
                        {
                            ParentState::Linked
                        } else {
                            ParentState::Ended
                        }
                    }
                };
                if Some(settled) != current {
                    updates.push((child_owner.clone(), child_ref.clone(), settled));
                }
            }
        }
        for (child_owner, child_ref, settled) in updates {
            let version = self.next_version();
            let Some(scope) = self
                .owners
                .get_mut(&child_owner)
                .and_then(|state| state.live.get_mut(&child_ref))
            else {
                continue;
            };
            let before = scope.tag();
            scope.parent_state = Some(settled);
            scope.version = version;
            let after = scope.tag();
            // A link settling to linked narrows nothing; one ending means no
            // live route may keep a stamp saying the parent is live.
            let drain = if settled == ParentState::Ended {
                ScopeDrain::All(RouteCloseReason::ScopeParentEnded)
            } else {
                ScopeDrain::Nothing
            };
            // The syncing owner's own scopes are already in its change list
            // with their pre-sync tag; keep that entry and move its `after`.
            if let Some(change) = tag_changes
                .iter_mut()
                .find(|change| change.owner == child_owner && change.scope_ref == child_ref)
            {
                change.after = Some(after);
                let current = std::mem::replace(&mut change.drain, ScopeDrain::Nothing);
                change.drain = current.widen(drain);
            } else {
                tag_changes.push(ScopeTagChange {
                    owner: child_owner,
                    scope_ref: child_ref,
                    before: Some(before),
                    after: Some(after),
                    drain,
                });
            }
        }
    }

    /// Admit a `route.open` naming a scope, or refuse it by name.
    ///
    /// `opener` is the route's attested principal, never anything from the
    /// request body; `target_module` is the module the route is opened to;
    /// `owner_configured` is whether the owner is in the supervisor's roster.
    /// The checks run in this order, each refusing with its own code: an epoch
    /// is named, the owner has synced since this daemon started, the ref is
    /// live, the named epoch is the live one, and the opener is the owner or a
    /// carrier permitted to reach `target_module`. Earlier checks come first
    /// so that a retryable refusal (owner not synced yet) is never masked by a
    /// terminal one that would only be true until the owner re-syncs.
    pub(crate) fn admit(
        &self,
        opener: &Principal,
        target_module: &str,
        selector: &ScopeSelector,
        owner_configured: bool,
    ) -> Result<ScopeAdmission, ScopeAdmissionRefusal> {
        let refuse = |code: &'static str, message: String| ScopeAdmissionRefusal { code, message };
        let Some(scope_epoch) = selector.scope_epoch else {
            return Err(refuse(
                error_codes::SCOPE_EPOCH_REQUIRED,
                "a scoped route.open must name the scope_epoch it serves".to_string(),
            ));
        };
        let scope_ref = &selector.scope_ref;
        let Some(owner) = reserved_module_id(&selector.owner) else {
            return Err(refuse(
                error_codes::SCOPE_NOT_LIVE,
                "a scope's owner is always a supervised module".to_string(),
            ));
        };
        let state = self.owners.get(owner).filter(|state| state.synced);
        let Some(state) = state else {
            return Err(if owner_configured {
                refuse(
                    error_codes::SCOPE_NOT_SYNCED,
                    format!("{owner} has not synced its scopes since this daemon started"),
                )
            } else {
                refuse(
                    error_codes::SCOPE_NOT_LIVE,
                    format!("{owner} is not a configured module and will never sync"),
                )
            });
        };
        let Some(scope) = state.live.get(scope_ref) else {
            return Err(refuse(
                error_codes::SCOPE_NOT_LIVE,
                format!("{owner} holds no live scope '{scope_ref}'"),
            ));
        };
        if scope.record.scope_epoch != scope_epoch {
            return Err(refuse(
                error_codes::SCOPE_ENDED,
                format!(
                    "scope '{scope_ref}' of {owner} is live at scope_epoch {}, not {scope_epoch}",
                    scope.record.scope_epoch
                ),
            ));
        }
        if reserved_module_id(opener) != Some(owner) {
            let permitted = match carrier_allowance(&scope.record, opener) {
                None => false,
                Some(None) => true,
                Some(Some(targets)) => targets.contains(target_module),
            };
            if !permitted {
                return Err(refuse(
                    error_codes::SCOPE_NOT_CARRIER,
                    format!(
                        "the opener is not the owner or a carrier of scope '{scope_ref}' of \
                         {owner} permitted to open to '{target_module}'"
                    ),
                ));
            }
        }
        Ok(ScopeAdmission {
            owner: owner.to_string(),
            stamp: ScopeStamp {
                owner: selector.owner.clone(),
                scope_ref: scope_ref.clone(),
                scope_epoch,
                kind: scope.record.kind,
                parent: scope.record.parent.clone(),
                parent_state: scope.parent_state,
                attributes: scope.record.attributes.clone(),
                owner_authorized: self.owner_authorized(owner),
            },
            tag: scope.tag(),
        })
    }

    /// The table's answer about one `(owner, ref)`.
    pub(crate) fn describe(&self, owner: &Principal, scope_ref: &str) -> ScopeDescription {
        let not_live = ScopeDescription {
            status: ScopeStatus::NotLive,
            scope_epoch: None,
            owner_synced: false,
            stamp: None,
        };
        let Some(owner_id) = reserved_module_id(owner) else {
            return not_live;
        };
        let Some(state) = self.owners.get(owner_id) else {
            return not_live;
        };
        if let Some(scope) = state.live.get(scope_ref) {
            return ScopeDescription {
                status: ScopeStatus::Live,
                scope_epoch: Some(scope.record.scope_epoch),
                owner_synced: state.synced,
                stamp: Some(ScopeStamp {
                    owner: owner.clone(),
                    scope_ref: scope_ref.to_string(),
                    scope_epoch: scope.record.scope_epoch,
                    kind: scope.record.kind,
                    parent: scope.record.parent.clone(),
                    parent_state: scope.parent_state,
                    attributes: scope.record.attributes.clone(),
                    owner_authorized: self.owner_authorized(owner_id),
                }),
            };
        }
        match state.tombstones.latest(scope_ref) {
            Some(epoch) => ScopeDescription {
                status: ScopeStatus::Ended,
                scope_epoch: Some(epoch),
                owner_synced: state.synced,
                stamp: None,
            },
            None => ScopeDescription {
                owner_synced: state.synced,
                ..not_live
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use subc_protocol::scope::{ScopeAttributes, ScopeCarrier, ScopeKind};

    use super::*;

    const PREFRONTAL: &str = "prefrontal-core";
    const MAGIC: &str = "magic-context";
    const BROCA: &str = "broca";
    const AFT: &str = "aft";

    fn conn(raw: u64) -> ConnectionId {
        ConnectionId::new(raw)
    }

    fn table() -> ScopeTable {
        ScopeTable::new([PREFRONTAL.to_string()])
    }

    fn record(scope_ref: &str, scope_epoch: u64, kind: ScopeKind) -> ScopeRecord {
        ScopeRecord {
            scope_ref: scope_ref.to_string(),
            scope_epoch,
            kind,
            parent: None,
            child_owners: Vec::new(),
            carriers: Vec::new(),
            attributes: ScopeAttributes::default(),
        }
    }

    fn head(scope_ref: &str, scope_epoch: u64) -> ScopeRecord {
        record(scope_ref, scope_epoch, ScopeKind::Head)
    }

    fn with_child_owner(mut record: ScopeRecord, owner: &str) -> ScopeRecord {
        record.child_owners.push(reserved(owner));
        record
    }

    fn child_of(
        scope_ref: &str,
        parent_owner: &str,
        parent_ref: &str,
        parent_epoch: u64,
    ) -> ScopeRecord {
        let mut record = record(scope_ref, 1, ScopeKind::Worker);
        record.parent = Some(ScopeParent {
            owner: reserved(parent_owner),
            scope_ref: parent_ref.to_string(),
            scope_epoch: parent_epoch,
        });
        record
    }

    /// Every connection counts as its owner's current launch. For tests about
    /// everything except authority.
    fn any_current(_: ConnectionId) -> bool {
        true
    }

    fn sync(
        table: &mut ScopeTable,
        owner: &str,
        connection: ConnectionId,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> SyncApplied {
        table
            .sync(owner, connection, any_current, generation, scopes)
            .unwrap_or_else(|refusal| panic!("sync for {owner} refused: {refusal:?}"))
    }

    fn outcome<'a>(applied: &'a SyncApplied, scope_ref: &str) -> &'a ScopeRecordResult {
        applied
            .results
            .iter()
            .find(|result| result.scope_ref == scope_ref)
            .unwrap_or_else(|| panic!("no result for {scope_ref}"))
    }

    fn describe(table: &ScopeTable, owner: &str, scope_ref: &str) -> ScopeDescription {
        table.describe(&reserved(owner), scope_ref)
    }

    fn live_epoch(table: &ScopeTable, owner: &str, scope_ref: &str) -> Option<u64> {
        let description = describe(table, owner, scope_ref);
        (description.status == ScopeStatus::Live)
            .then_some(description.scope_epoch)
            .flatten()
    }

    fn parent_state(table: &ScopeTable, owner: &str, scope_ref: &str) -> Option<ParentState> {
        describe(table, owner, scope_ref)
            .stamp
            .and_then(|stamp| stamp.parent_state)
    }

    fn version(table: &ScopeTable, owner: &str, scope_ref: &str) -> u64 {
        table.owners[owner].live[scope_ref].version
    }

    #[test]
    fn hello_launch_nonces_debug_prints_no_nonce() {
        let mut nonces = HelloLaunchNonces::default();
        nonces.record(conn(1), Some("secret-nonce-one"));
        nonces.record(conn(2), Some("secret-nonce-two"));
        let printed = format!("{nonces:?}");
        assert!(!printed.contains("secret-nonce"), "{printed}");
        // Control: the instance is populated, so an empty map is not why no
        // nonce was printed.
        assert!(printed.contains('2'), "{printed}");
        assert!(nonces.presented(conn(1), Some("secret-nonce-one")));
    }

    // ---- ownership -------------------------------------------------------

    #[test]
    fn the_same_ref_under_two_owners_is_two_scopes() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        sync(
            &mut table,
            BROCA,
            conn(2),
            1,
            vec![record("s", 9, ScopeKind::Worker)],
        );
        assert_eq!(live_epoch(&table, PREFRONTAL, "s"), Some(5));
        assert_eq!(live_epoch(&table, BROCA, "s"), Some(9));

        // Removing it from one owner's set leaves the other owner's alone.
        sync(&mut table, BROCA, conn(2), 2, Vec::new());
        assert_eq!(describe(&table, BROCA, "s").status, ScopeStatus::Ended);
        assert_eq!(live_epoch(&table, PREFRONTAL, "s"), Some(5));
    }

    #[test]
    fn a_non_module_principal_owns_nothing() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        for principal in [Principal::Direct, Principal::Unverified] {
            let description = table.describe(&principal, "s");
            assert_eq!(description.status, ScopeStatus::NotLive);
            assert!(!description.owner_synced);
        }
    }

    // ---- the authority gate -------------------------------------------------

    #[test]
    fn a_gated_attribute_from_an_unlisted_owner_is_refused() {
        let mut table = table();
        let mut gated = head("h", 1);
        gated.attributes = ScopeAttributes {
            agent_id: Some("agent".to_string()),
            delegates: false,
        };
        let mut delegating = head("d", 1);
        delegating.attributes.delegates = true;

        let applied = sync(
            &mut table,
            BROCA,
            conn(2),
            1,
            vec![gated.clone(), delegating, head("plain", 1)],
        );
        assert_eq!(outcome(&applied, "h").outcome, ScopeRecordOutcome::Refused);
        assert_eq!(
            outcome(&applied, "h").code.as_deref(),
            Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
        );
        assert_eq!(
            outcome(&applied, "d").code.as_deref(),
            Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
        );
        assert_eq!(
            outcome(&applied, "plain").outcome,
            ScopeRecordOutcome::Created
        );
        assert_eq!(describe(&table, BROCA, "h").status, ScopeStatus::NotLive);

        // The listed owner may set it, and only its stamp is owner_authorized.
        let applied = sync(&mut table, PREFRONTAL, conn(1), 1, vec![gated]);
        assert_eq!(outcome(&applied, "h").outcome, ScopeRecordOutcome::Created);
        let stamp = describe(&table, PREFRONTAL, "h").stamp.unwrap();
        assert!(stamp.owner_authorized);
        assert_eq!(stamp.attributes.agent_id.as_deref(), Some("agent"));
        assert!(
            !describe(&table, BROCA, "plain")
                .stamp
                .unwrap()
                .owner_authorized
        );
    }

    #[test]
    fn delegates_without_an_agent_and_an_empty_target_list_are_refused_by_name() {
        let mut table = table();
        let mut delegating = head("d", 1);
        delegating.attributes.delegates = true;
        let mut empty_targets = head("t", 1);
        empty_targets.carriers.push(ScopeCarrier {
            principal: reserved(AFT),
            targets: Some(Vec::new()),
        });
        let mut too_many_targets = head("u", 1);
        too_many_targets.carriers.push(ScopeCarrier {
            principal: reserved(AFT),
            targets: Some((0..=MAX_CARRIER_TARGETS).map(|i| format!("m{i}")).collect()),
        });
        let mut targeted = head("ok", 1);
        targeted.carriers.push(ScopeCarrier {
            principal: reserved(AFT),
            targets: Some(vec!["plexus".to_string()]),
        });
        let applied = sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            1,
            vec![delegating, empty_targets, too_many_targets, targeted],
        );
        assert_eq!(
            outcome(&applied, "d").code.as_deref(),
            Some(error_codes::SCOPE_DELEGATES_WITHOUT_AGENT)
        );
        for scope_ref in ["t", "u"] {
            assert_eq!(
                outcome(&applied, scope_ref).code.as_deref(),
                Some(error_codes::SCOPE_CARRIER_TARGETS_INVALID),
                "{scope_ref}"
            );
        }
        assert_eq!(outcome(&applied, "ok").outcome, ScopeRecordOutcome::Created);
    }

    // ---- sync authority -----------------------------------------------------

    #[test]
    fn a_connection_that_is_not_the_owners_current_launch_cannot_sync() {
        let mut table = table();
        let refusal = table
            .sync(PREFRONTAL, conn(1), |_| false, 1, vec![head("s", 1)])
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        let description = describe(&table, PREFRONTAL, "s");
        assert_eq!(description.status, ScopeStatus::NotLive);
        assert!(
            !description.owner_synced,
            "a refused sync does not count as the owner having synced"
        );
    }

    /// A blue/green swap, driven through the one fact the table sees: which
    /// connection holds the module's recorded spawn nonce.
    #[test]
    fn a_swap_candidate_never_takes_authority_and_the_serving_owner_syncs_throughout() {
        let mut table = table();
        let incumbent = conn(1);
        let candidate = conn(2);
        // Before cutover the recorded nonce is still the incumbent's; the
        // candidate holds the swap token.
        let before_cutover = |c: ConnectionId| c == incumbent;

        table
            .sync(PREFRONTAL, incumbent, before_cutover, 1, vec![head("s", 1)])
            .unwrap();
        let refusal = table
            .sync(
                PREFRONTAL,
                candidate,
                before_cutover,
                99,
                vec![head("x", 1)],
            )
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        assert_eq!(live_epoch(&table, PREFRONTAL, "x"), None);
        table
            .sync(
                PREFRONTAL,
                incumbent,
                before_cutover,
                2,
                vec![head("s", 1), head("t", 1)],
            )
            .expect("the serving owner's sync is accepted while a candidate exists");

        // The candidate fails and is rolled back: its token never became the
        // recorded nonce, so it still cannot sync, and the incumbent can.
        let refusal = table
            .sync(PREFRONTAL, candidate, before_cutover, 100, Vec::new())
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        assert_eq!(live_epoch(&table, PREFRONTAL, "t"), Some(1));
        table
            .sync(PREFRONTAL, incumbent, before_cutover, 3, vec![head("s", 1)])
            .expect("the serving owner's sync is accepted after the rollback");

        // A second swap that cuts over: the promoted candidate's nonce becomes
        // the recorded one. Moving authority does not touch the set; the
        // promoted connection's first sync replaces it at any generation, and
        // the superseded incumbent is refused from then on.
        let promoted = conn(3);
        let after_cutover = |c: ConnectionId| c == promoted;
        let before = describe(&table, PREFRONTAL, "s");
        let refusal = table
            .sync(PREFRONTAL, incumbent, after_cutover, 4, vec![head("s", 1)])
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        assert_eq!(describe(&table, PREFRONTAL, "s"), before);
        table
            .sync(PREFRONTAL, promoted, after_cutover, 1, vec![head("s", 1)])
            .expect("the promoted launch takes authority at any generation");
        let refusal = table
            .sync(PREFRONTAL, incumbent, after_cutover, 50, Vec::new())
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        assert_eq!(live_epoch(&table, PREFRONTAL, "s"), Some(1));
    }

    #[test]
    fn a_newer_launch_takes_authority_from_an_older_launchs_open_connection() {
        let mut table = table();
        let wedged = conn(1);
        let restarted = conn(2);
        table
            .sync(PREFRONTAL, wedged, |c| c == wedged, 10, vec![head("s", 1)])
            .unwrap();
        // The owner restarted: its new process holds the recorded nonce while
        // the old connection is still open.
        let now = |c: ConnectionId| c == restarted;
        table
            .sync(PREFRONTAL, restarted, now, 1, vec![head("t", 1)])
            .expect("the current launch takes authority from the older one");
        assert_eq!(live_epoch(&table, PREFRONTAL, "t"), Some(1));
        assert_eq!(describe(&table, PREFRONTAL, "s").status, ScopeStatus::Ended);
        let refusal = table
            .sync(PREFRONTAL, wedged, now, 11, vec![head("s", 2)])
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
    }

    #[test]
    fn a_second_connection_of_the_current_launch_is_refused() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 1)]);
        let refusal = table
            .sync(PREFRONTAL, conn(2), any_current, 2, Vec::new())
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_NOT_AUTHORITY);
        assert_eq!(live_epoch(&table, PREFRONTAL, "s"), Some(1));

        // Once the authority's connection closes, the next connection takes it.
        table.release_connection(conn(1));
        sync(&mut table, PREFRONTAL, conn(2), 1, vec![head("s", 1)]);
    }

    // ---- generations --------------------------------------------------------

    #[test]
    fn a_restarted_owners_first_sync_replaces_at_any_generation_and_a_later_equal_or_smaller_one_is_refused_unchanged(
    ) {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 100, vec![head("old", 1)]);
        table.release_connection(conn(1));

        // A restarted owner starts its generations again; its first sync on the
        // new connection replaces the set whatever the number.
        let applied = sync(&mut table, PREFRONTAL, conn(2), 3, vec![head("new", 1)]);
        assert_eq!(applied.ended.len(), 1);
        assert_eq!(live_epoch(&table, PREFRONTAL, "new"), Some(1));
        assert_eq!(
            describe(&table, PREFRONTAL, "old").status,
            ScopeStatus::Ended
        );

        for stale in [3, 2] {
            let refusal = table
                .sync(PREFRONTAL, conn(2), any_current, stale, Vec::new())
                .unwrap_err();
            assert_eq!(
                refusal.code,
                error_codes::SCOPE_SYNC_STALE,
                "generation {stale}"
            );
            assert_eq!(
                live_epoch(&table, PREFRONTAL, "new"),
                Some(1),
                "a stale sync removed nothing"
            );
        }
        // A stale sync refuses even records that would otherwise apply.
        let refusal = table
            .sync(
                PREFRONTAL,
                conn(2),
                any_current,
                1,
                vec![head("new", 1), head("x", 1)],
            )
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_SYNC_STALE);
        assert_eq!(
            describe(&table, PREFRONTAL, "x").status,
            ScopeStatus::NotLive
        );
        sync(&mut table, PREFRONTAL, conn(2), 4, vec![head("new", 1)]);
    }

    // ---- epochs and kinds ---------------------------------------------------

    #[test]
    fn a_higher_epoch_ends_the_old_scope_and_a_lower_one_is_refused() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        let first = version(&table, PREFRONTAL, "s");

        let applied = sync(&mut table, PREFRONTAL, conn(1), 2, vec![head("s", 6)]);
        assert_eq!(outcome(&applied, "s").outcome, ScopeRecordOutcome::Replaced);
        assert_eq!(
            applied.ended,
            vec![ScopeEnded {
                scope_ref: "s".to_string(),
                scope_epoch: 5
            }]
        );
        assert_ne!(version(&table, PREFRONTAL, "s"), first);
        assert_eq!(
            applied.tag_changes,
            vec![ScopeTagChange {
                owner: PREFRONTAL.to_string(),
                scope_ref: "s".to_string(),
                before: Some(ScopeTag {
                    scope_epoch: 5,
                    version: first
                }),
                after: Some(ScopeTag {
                    scope_epoch: 6,
                    version: version(&table, PREFRONTAL, "s")
                }),
                drain: ScopeDrain::All(RouteCloseReason::ScopeEnded),
            }]
        );

        let applied = sync(&mut table, PREFRONTAL, conn(1), 3, vec![head("s", 4)]);
        assert_eq!(
            outcome(&applied, "s").code.as_deref(),
            Some(error_codes::SCOPE_EPOCH_REGRESSED)
        );
        assert_eq!(live_epoch(&table, PREFRONTAL, "s"), Some(6));
    }

    #[test]
    fn a_kind_change_at_the_same_epoch_is_refused() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        let applied = sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            2,
            vec![record("s", 5, ScopeKind::Ephemeral)],
        );
        assert_eq!(
            outcome(&applied, "s").code.as_deref(),
            Some(error_codes::SCOPE_KIND_CHANGED)
        );
        assert_eq!(
            describe(&table, PREFRONTAL, "s").stamp.unwrap().kind,
            ScopeKind::Head
        );
        // A higher epoch may change it.
        let applied = sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            3,
            vec![record("s", 6, ScopeKind::Ephemeral)],
        );
        assert_eq!(outcome(&applied, "s").outcome, ScopeRecordOutcome::Replaced);
    }

    #[test]
    fn a_resent_tombstoned_epoch_is_refused() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        sync(&mut table, PREFRONTAL, conn(1), 2, Vec::new());
        let applied = sync(&mut table, PREFRONTAL, conn(1), 3, vec![head("s", 5)]);
        assert_eq!(
            outcome(&applied, "s").code.as_deref(),
            Some(error_codes::SCOPE_EPOCH_ENDED)
        );
        assert_eq!(describe(&table, PREFRONTAL, "s").status, ScopeStatus::Ended);

        // An older session of the ref cannot come back after a newer one ended.
        let applied = sync(&mut table, PREFRONTAL, conn(1), 4, vec![head("s", 4)]);
        assert_eq!(
            outcome(&applied, "s").code.as_deref(),
            Some(error_codes::SCOPE_EPOCH_REGRESSED)
        );
        let applied = sync(&mut table, PREFRONTAL, conn(1), 5, vec![head("s", 6)]);
        assert_eq!(outcome(&applied, "s").outcome, ScopeRecordOutcome::Created);
    }

    // ---- versions and partial application ------------------------------------

    #[test]
    fn an_unchanged_record_does_not_move_its_version() {
        let mut table = table();
        let mut scope = head("s", 1);
        scope.carriers.push(ScopeCarrier {
            principal: reserved(BROCA),
            targets: None,
        });
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![scope.clone()]);
        let before = version(&table, PREFRONTAL, "s");

        let applied = sync(&mut table, PREFRONTAL, conn(1), 2, vec![scope.clone()]);
        assert_eq!(
            outcome(&applied, "s").outcome,
            ScopeRecordOutcome::Unchanged
        );
        assert_eq!(outcome(&applied, "s").version, Some(before));
        assert_eq!(version(&table, PREFRONTAL, "s"), before);
        assert!(applied.tag_changes.is_empty(), "{:?}", applied.tag_changes);

        // Control: a content change does move it.
        scope.carriers.clear();
        let applied = sync(&mut table, PREFRONTAL, conn(1), 3, vec![scope]);
        assert_eq!(outcome(&applied, "s").outcome, ScopeRecordOutcome::Updated);
        assert!(version(&table, PREFRONTAL, "s") > before);
    }

    #[test]
    fn a_refused_record_keeps_its_previous_state_while_the_rest_apply() {
        let mut table = table();
        let mut kept = head("kept", 5);
        kept.carriers.push(ScopeCarrier {
            principal: reserved(BROCA),
            targets: None,
        });
        sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            1,
            vec![kept.clone(), head("other", 1)],
        );
        let kept_version = version(&table, PREFRONTAL, "kept");

        // The refused record is regressed AND drops its carrier; neither
        // applies. The other record changes, and a new one is created.
        let mut regressed = head("kept", 4);
        regressed.carriers.clear();
        let mut other = head("other", 1);
        other.child_owners.push(reserved(MAGIC));
        let applied = sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            2,
            vec![regressed, other, head("new", 1)],
        );
        let refused = outcome(&applied, "kept");
        assert_eq!(refused.outcome, ScopeRecordOutcome::Refused);
        assert_eq!(refused.version, Some(kept_version));
        assert!(
            applied.ended.is_empty(),
            "a refused record is not a removal"
        );
        let stamp_epoch = live_epoch(&table, PREFRONTAL, "kept");
        assert_eq!(stamp_epoch, Some(5));
        assert_eq!(table.owners[PREFRONTAL].live["kept"].record, kept);
        assert_eq!(
            outcome(&applied, "other").outcome,
            ScopeRecordOutcome::Updated
        );
        assert_eq!(
            outcome(&applied, "new").outcome,
            ScopeRecordOutcome::Created
        );
    }

    // ---- parents ------------------------------------------------------------

    #[test]
    fn a_parent_is_accepted_only_from_its_owner_or_a_child_owner_at_its_live_epoch() {
        let mut table = table();
        let mut parent = with_child_owner(head("h", 3), MAGIC);
        parent.carriers.push(ScopeCarrier {
            principal: reserved(AFT),
            targets: None,
        });
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![parent.clone()]);

        // The parent's owner.
        let applied = sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            2,
            vec![parent.clone(), child_of("own-child", PREFRONTAL, "h", 3)],
        );
        assert_eq!(
            outcome(&applied, "own-child").parent_state,
            Some(ParentState::Linked)
        );

        // A principal in child_owners, at the live epoch.
        let applied = sync(
            &mut table,
            MAGIC,
            conn(2),
            1,
            vec![child_of("hist", PREFRONTAL, "h", 3)],
        );
        assert_eq!(
            outcome(&applied, "hist").outcome,
            ScopeRecordOutcome::Created
        );
        assert_eq!(
            parent_state(&table, MAGIC, "hist"),
            Some(ParentState::Linked)
        );

        // A child owner naming another epoch is refused.
        let applied = sync(
            &mut table,
            MAGIC,
            conn(2),
            2,
            vec![
                child_of("hist", PREFRONTAL, "h", 3),
                child_of("stale", PREFRONTAL, "h", 2),
            ],
        );
        assert_eq!(
            outcome(&applied, "stale").code.as_deref(),
            Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
        );

        // A carrier of the parent is not thereby a child owner.
        let applied = sync(
            &mut table,
            AFT,
            conn(3),
            1,
            vec![child_of("c", PREFRONTAL, "h", 3)],
        );
        assert_eq!(
            outcome(&applied, "c").code.as_deref(),
            Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
        );
        assert_eq!(describe(&table, AFT, "c").status, ScopeStatus::NotLive);

        // Neither is an owner that is not listed at all.
        let applied = sync(
            &mut table,
            BROCA,
            conn(4),
            1,
            vec![child_of("c", PREFRONTAL, "h", 3)],
        );
        assert_eq!(
            outcome(&applied, "c").code.as_deref(),
            Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
        );
    }

    #[test]
    fn a_cycle_is_refused() {
        let mut table = table();
        // Within one owner: a names b and b names a.
        let mut a = with_child_owner(head("a", 1), MAGIC);
        a.parent = Some(ScopeParent {
            owner: reserved(PREFRONTAL),
            scope_ref: "b".to_string(),
            scope_epoch: 1,
        });
        let mut b = head("b", 1);
        b.parent = Some(ScopeParent {
            owner: reserved(PREFRONTAL),
            scope_ref: "a".to_string(),
            scope_epoch: 1,
        });
        let applied = sync(&mut table, PREFRONTAL, conn(1), 1, vec![a, b]);
        let refused = applied
            .results
            .iter()
            .filter(|result| {
                result.code.as_deref() == Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
            })
            .count();
        assert!(refused >= 1, "{:?}", applied.results);

        // A scope naming itself.
        let mut own = head("self", 1);
        own.parent = Some(ScopeParent {
            owner: reserved(PREFRONTAL),
            scope_ref: "self".to_string(),
            scope_epoch: 1,
        });
        let applied = sync(&mut table, PREFRONTAL, conn(1), 2, vec![own]);
        assert_eq!(
            outcome(&applied, "self").code.as_deref(),
            Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
        );

        // Across owners: magic's m names prefrontal's p while prefrontal has not
        // synced (pending); prefrontal then syncs p naming m.
        let mut table = super::tests::table();
        let mut m = with_child_owner(child_of("m", PREFRONTAL, "p", 1), PREFRONTAL);
        m.scope_epoch = 1;
        sync(&mut table, MAGIC, conn(2), 1, vec![m]);
        assert_eq!(parent_state(&table, MAGIC, "m"), Some(ParentState::Pending));
        let mut p = with_child_owner(head("p", 1), MAGIC);
        p.parent = Some(ScopeParent {
            owner: reserved(MAGIC),
            scope_ref: "m".to_string(),
            scope_epoch: 1,
        });
        let applied = sync(&mut table, PREFRONTAL, conn(1), 1, vec![p]);
        assert_eq!(
            outcome(&applied, "p").code.as_deref(),
            Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)
        );
        // p was refused, so m's parent is absent in prefrontal's synced set.
        assert_eq!(parent_state(&table, MAGIC, "m"), Some(ParentState::Ended));
    }

    #[test]
    fn leaf_first_parent_chains_use_bounded_link_work() {
        for size in [100, 1000] {
            for refuse_root in [false, true] {
                let mut table = table();
                let mut root = head("0", 1);
                if refuse_root {
                    root.attributes.delegates = true;
                }
                let mut records = (1..size)
                    .rev()
                    .map(|index| {
                        child_of(&index.to_string(), PREFRONTAL, &(index - 1).to_string(), 1)
                    })
                    .collect::<Vec<_>>();
                records.push(root);
                let applied = sync(&mut table, PREFRONTAL, conn(1), 1, records);
                assert!(applied.results.iter().all(|result| result.outcome
                    == if refuse_root {
                        ScopeRecordOutcome::Refused
                    } else {
                        ScopeRecordOutcome::Created
                    }));
                let lookups = table
                    .link_lookups
                    .load(std::sync::atomic::Ordering::Relaxed);
                assert!(
                    lookups <= size * 20,
                    "{size} records used {lookups} parent lookups (refused root: {refuse_root})"
                );
            }
        }
    }

    /// Every order in which a parent's owner and a child's owner can sync after
    /// a daemon restart. The child is never refused for the order; the link
    /// settles when the parent's owner syncs.
    #[test]
    fn every_ordering_of_parent_and_child_owner_syncs_settles_the_link() {
        struct Case {
            name: &'static str,
            parent_first: bool,
            parent_set: Vec<ScopeRecord>,
            expected: ParentState,
        }
        let permitted = with_child_owner(head("h", 3), MAGIC);
        let cases = [
            Case {
                name: "parent owner first, parent live and permitted",
                parent_first: true,
                parent_set: vec![permitted.clone()],
                expected: ParentState::Linked,
            },
            Case {
                name: "child owner first, parent then live and permitted",
                parent_first: false,
                parent_set: vec![permitted.clone()],
                expected: ParentState::Linked,
            },
            Case {
                name: "child owner first, parent then absent",
                parent_first: false,
                parent_set: Vec::new(),
                expected: ParentState::Ended,
            },
            Case {
                name: "child owner first, parent then at another epoch",
                parent_first: false,
                parent_set: vec![with_child_owner(head("h", 4), MAGIC)],
                expected: ParentState::Ended,
            },
            Case {
                name: "child owner first, parent then live but the link not permitted",
                parent_first: false,
                parent_set: vec![head("h", 3)],
                expected: ParentState::Ended,
            },
        ];
        for case in cases {
            let mut table = table();
            if case.parent_first {
                sync(&mut table, PREFRONTAL, conn(1), 1, case.parent_set.clone());
            }
            let applied = sync(
                &mut table,
                MAGIC,
                conn(2),
                1,
                vec![child_of("w", PREFRONTAL, "h", 3)],
            );
            assert_ne!(
                outcome(&applied, "w").outcome,
                ScopeRecordOutcome::Refused,
                "{}: a child is never refused for the order",
                case.name
            );
            if !case.parent_first {
                assert_eq!(
                    parent_state(&table, MAGIC, "w"),
                    Some(ParentState::Pending),
                    "{}",
                    case.name
                );
                let before = version(&table, MAGIC, "w");
                let applied = sync(&mut table, PREFRONTAL, conn(1), 1, case.parent_set.clone());
                // The settled link is a content change of the child, which a
                // bind in flight must notice.
                assert!(version(&table, MAGIC, "w") > before, "{}", case.name);
                assert!(
                    applied
                        .tag_changes
                        .iter()
                        .any(|change| change.owner == MAGIC && change.scope_ref == "w"),
                    "{}: {:?}",
                    case.name,
                    applied.tag_changes
                );
            }
            assert_eq!(
                parent_state(&table, MAGIC, "w"),
                Some(case.expected),
                "{}",
                case.name
            );
            assert_eq!(
                live_epoch(&table, MAGIC, "w"),
                Some(1),
                "{}: the child stays live whatever the link settles to",
                case.name
            );
        }
    }

    #[test]
    fn a_parent_ending_ends_its_childrens_links_and_a_new_session_never_adopts_them() {
        let mut table = table();
        let parent = with_child_owner(head("h", 3), MAGIC);
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![parent]);
        sync(
            &mut table,
            MAGIC,
            conn(2),
            1,
            vec![child_of("w", PREFRONTAL, "h", 3)],
        );
        assert_eq!(parent_state(&table, MAGIC, "w"), Some(ParentState::Linked));

        // A new session under the parent's ref.
        sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            2,
            vec![with_child_owner(head("h", 4), MAGIC)],
        );
        assert_eq!(parent_state(&table, MAGIC, "w"), Some(ParentState::Ended));

        // Re-sending the child unchanged is not a new link, so it is neither
        // refused nor revived.
        let applied = sync(
            &mut table,
            MAGIC,
            conn(2),
            2,
            vec![child_of("w", PREFRONTAL, "h", 3)],
        );
        assert_eq!(
            outcome(&applied, "w").outcome,
            ScopeRecordOutcome::Unchanged
        );
        assert_eq!(parent_state(&table, MAGIC, "w"), Some(ParentState::Ended));
    }

    #[test]
    fn a_same_owner_parent_removed_in_the_same_sync_ends_the_childs_link() {
        let mut table = table();
        let child = child_of("w", PREFRONTAL, "h", 3);
        sync(
            &mut table,
            PREFRONTAL,
            conn(1),
            1,
            vec![head("h", 3), child.clone()],
        );
        assert_eq!(
            parent_state(&table, PREFRONTAL, "w"),
            Some(ParentState::Linked)
        );
        let applied = sync(&mut table, PREFRONTAL, conn(1), 2, vec![child]);
        assert_eq!(outcome(&applied, "w").outcome, ScopeRecordOutcome::Updated);
        assert_eq!(
            outcome(&applied, "w").parent_state,
            Some(ParentState::Ended)
        );
    }

    // ---- describe -----------------------------------------------------------

    /// The answers a reader holding a binding to one session tells apart: live
    /// at an epoch it compares with its own, ended, not live after the owner
    /// synced (gone), and not live before the owner synced (wait). Whether the
    /// owner is in the supervisor's roster, which separates "wait" from "will
    /// never sync", is added by the control handler and tested there.
    #[test]
    fn describe_separates_the_reader_cases() {
        let mut table = table();
        // Owner has not synced: not live, and owner_synced false.
        let description = describe(&table, PREFRONTAL, "s");
        assert_eq!(description.status, ScopeStatus::NotLive);
        assert!(!description.owner_synced);

        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("s", 5)]);
        // Live at epoch 5. A reader holding epoch 5 uses it; one holding any
        // other epoch learns its session was replaced.
        let description = describe(&table, PREFRONTAL, "s");
        assert_eq!(description.status, ScopeStatus::Live);
        assert_eq!(description.scope_epoch, Some(5));
        assert!(description.owner_synced);
        let stamp = description.stamp.unwrap();
        assert_eq!((stamp.scope_epoch, stamp.kind), (5, ScopeKind::Head));

        // Removed by a sync: ended, with the epoch that ended.
        sync(&mut table, PREFRONTAL, conn(1), 2, Vec::new());
        let description = describe(&table, PREFRONTAL, "s");
        assert_eq!(description.status, ScopeStatus::Ended);
        assert_eq!(description.scope_epoch, Some(5));
        assert!(description.stamp.is_none());

        // Never synced by an owner that has synced: not live, owner_synced
        // true, which a reader treats as gone.
        let description = describe(&table, PREFRONTAL, "never");
        assert_eq!(description.status, ScopeStatus::NotLive);
        assert!(description.owner_synced);
    }

    // ---- bounds -------------------------------------------------------------

    #[test]
    fn the_live_scope_bound_refuses_the_whole_sync() {
        let mut table = table();
        sync(&mut table, BROCA, conn(1), 1, vec![head("keep", 1)]);
        let too_many = (0..=MAX_LIVE_SCOPES_PER_OWNER)
            .map(|i| head(&format!("s{i}"), 1))
            .collect();
        let refusal = table
            .sync(BROCA, conn(1), any_current, 2, too_many)
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_LIVE_LIMIT_EXCEEDED);
        assert_eq!(live_epoch(&table, BROCA, "keep"), Some(1));
        assert_eq!(describe(&table, BROCA, "s0").status, ScopeStatus::NotLive);

        // Control: exactly the bound is accepted.
        let at_bound = (0..MAX_LIVE_SCOPES_PER_OWNER)
            .map(|i| head(&format!("s{i}"), 1))
            .collect();
        sync(&mut table, BROCA, conn(1), 3, at_bound);
    }

    #[test]
    fn the_attribute_bound_refuses_the_whole_sync() {
        let mut table = table();
        sync(&mut table, PREFRONTAL, conn(1), 1, vec![head("keep", 1)]);
        let mut large = head("large", 1);
        large.attributes.agent_id = Some("a".repeat(MAX_SCOPE_ATTRIBUTE_BYTES));
        let refusal = table
            .sync(PREFRONTAL, conn(1), any_current, 2, vec![large])
            .unwrap_err();
        assert_eq!(refusal.code, error_codes::SCOPE_ATTRIBUTES_TOO_LARGE);
        assert_eq!(live_epoch(&table, PREFRONTAL, "keep"), Some(1));

        // Control: attributes that fit are accepted.
        let mut fits = head("fits", 1);
        fits.attributes.agent_id = Some("a".repeat(MAX_SCOPE_ATTRIBUTE_BYTES - 64));
        sync(&mut table, PREFRONTAL, conn(1), 3, vec![fits]);
    }

    #[test]
    fn the_tombstone_bound_evicts_the_oldest_and_never_refuses() {
        let mut table = table();
        let count = MAX_SCOPE_TOMBSTONES_PER_OWNER + 5;
        let all = (0..count).map(|i| head(&format!("s{i}"), 1)).collect();
        sync(&mut table, BROCA, conn(1), 1, all);
        let applied = sync(&mut table, BROCA, conn(1), 2, Vec::new());
        assert_eq!(applied.ended.len(), count, "every removal applies");
        assert_eq!(
            table.owners[BROCA].tombstones.len(),
            MAX_SCOPE_TOMBSTONES_PER_OWNER
        );
        // The tombstones of the five oldest removals were evicted: those scopes
        // now read not_live (after the owner's sync a reader treats that as
        // gone), while the retained ones read ended.
        assert_eq!(describe(&table, BROCA, "s0").status, ScopeStatus::NotLive);
        assert_eq!(describe(&table, BROCA, "s5").status, ScopeStatus::Ended);
        assert_eq!(
            describe(&table, BROCA, &format!("s{}", count - 1)).status,
            ScopeStatus::Ended
        );
    }

    #[test]
    fn a_duplicate_or_empty_ref_refuses_the_whole_sync() {
        let mut table = table();
        for scopes in [vec![head("a", 1), head("a", 2)], vec![head("", 1)]] {
            let refusal = table
                .sync(PREFRONTAL, conn(1), any_current, 1, scopes)
                .unwrap_err();
            assert_eq!(refusal.code, INVALID_CONTROL_BODY);
        }
        assert!(!describe(&table, PREFRONTAL, "a").owner_synced);
    }
}
