use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

use subc_protocol::manifest::{CapabilityDeclarations, ModuleManifest, ProviderRole};
use tokio::sync::watch;

/// Per-connection identity assigned by [`crate::Router`] while serving a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Synthetic connection id used by unit tests. Router-issued connection ids
    /// start at 1, so this 0 value never collides with a real socket owner.
    #[cfg(test)]
    pub const LOCAL: Self = Self(0);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Lifecycle state for a module's channel allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Active,
    Closed,
}

/// Registry record for one active module registration.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleRegistration {
    pub manifest: ModuleManifest,
    pub ready: bool,
    pub negotiated_ver: u8,
    pub state: ChannelState,
    pub connection_id: ConnectionId,
    pub control_ops: Vec<String>,
}

/// Which registration a lookup or a lifecycle wait is about.
///
/// A module id alone stops naming one process once a blue/green swap runs two
/// processes under the same id: the incumbent in the active slot, the
/// replacement in the candidate slot, and, after cutover, the old incumbent
/// demoted until its connection goes away. A wait keyed on the bare id would
/// confuse them (a successful swap never empties the id's active slot, and a
/// candidate's "has it registered yet" would be answered by the incumbent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationSlot<'a> {
    /// The routable registration for this module id. This is what every plain
    /// start, stop and restart path means by "the module is registered".
    Active(&'a str),
    /// A swap candidate registered under this module id and not yet promoted.
    Candidate(&'a str),
    /// Whatever registration this module connection holds, in any slot.
    Connection(ConnectionId),
}

/// The result of promoting a module's candidate registration to active.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryCutover {
    /// The registration that is now active (the former candidate).
    pub promoted: ModuleRegistration,
    /// The former active registration, now demoted. `None` when the incumbent
    /// had already gone away before the promotion.
    pub superseded: Option<ModuleRegistration>,
}

/// Control-plane registry for module manifests and supervision ownership.
///
/// Duplicate active `module_id`s are rejected rather than replaced. Rejection is
/// the safer v1 behavior because replacing a still-connected module could hijack
/// in-flight routes. Stale registrations are removed by connection cleanup; a
/// reconnect after the old connection drops can then register the same id again.
///
/// A blue/green swap is the one sanctioned way two processes hold the same id.
/// The replacement registers into a separate candidate slot, which every by-id
/// lookup ignores, so it stays unroutable until [`Registry::promote_candidate`]
/// swaps it in. The old incumbent is then kept as "superseded" until its
/// connection deregisters. Lookups keyed by connection id search all three
/// slots, because a connection must be able to update and remove its own
/// registration whichever slot it currently sits in.
#[derive(Debug, Default)]
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    /// Only subscribed module ids have a sender. Changes coalesce, and writers
    /// never wait for a supervisor to receive them.
    module_changes: HashMap<String, watch::Sender<()>>,
    modules: HashMap<String, ModuleRegistration>,
    /// Swap candidates by module id: registered, never routable, never listed.
    candidates: HashMap<String, ModuleRegistration>,
    /// Former incumbents demoted by a promotion, kept only so their own
    /// connection can still find and remove them. Never routable, never listed.
    superseded: Vec<ModuleRegistration>,
    generation: u64,
}

impl Registry {
    /// Subscribe before reading a registration so a change between the read
    /// and the supervisor's wait remains pending on this receiver.
    pub(crate) fn subscribe_module_changes(
        &self,
        module_id: &str,
    ) -> Result<watch::Receiver<()>, RegistryError> {
        let mut inner = self.lock_inner()?;
        // Retired supervisors must not leave an ever-growing set of ids.
        inner
            .module_changes
            .retain(|_, sender| sender.receiver_count() > 0);
        Ok(inner
            .module_changes
            .entry(module_id.to_string())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe())
    }

    /// Register a module manifest with the module's effective granted control op set.
    pub fn register_with_control_ops(
        &self,
        manifest: ModuleManifest,
        negotiated_ver: u8,
        connection_id: ConnectionId,
        control_ops: Vec<String>,
    ) -> Result<ModuleRegistration, RegistryError> {
        let module_id = manifest.module_id.clone();
        if let Err(reason) = module_id_path_hazard(&module_id) {
            return Err(RegistryError::PathHazardModuleId { module_id, reason });
        }
        let mut inner = self.lock_inner()?;
        if inner.modules.contains_key(&module_id) {
            return Err(RegistryError::DuplicateModuleId { module_id });
        }

        let ready = manifest.ready.unwrap_or(true);
        let registration = ModuleRegistration {
            manifest,
            ready,
            negotiated_ver,
            state: ChannelState::Active,
            connection_id,
            control_ops,
        };

        inner.modules.insert(module_id, registration.clone());
        inner.bump_generation();
        inner.notify_module_changed(&registration.manifest.module_id);
        Ok(registration)
    }

    /// Register a swap candidate for `manifest.module_id` into the candidate slot.
    ///
    /// The candidate is invisible to [`Self::get_module`], [`Self::list_modules`]
    /// and every other by-id lookup until [`Self::promote_candidate`]. An active
    /// registration for the id is not required, because the incumbent may die
    /// while the swap is open; deciding whether a candidate may register at all
    /// belongs to the caller that admits it. A second candidate for the same id
    /// is refused.
    pub fn register_candidate_with_control_ops(
        &self,
        manifest: ModuleManifest,
        negotiated_ver: u8,
        connection_id: ConnectionId,
        control_ops: Vec<String>,
    ) -> Result<ModuleRegistration, RegistryError> {
        let module_id = manifest.module_id.clone();
        if let Err(reason) = module_id_path_hazard(&module_id) {
            return Err(RegistryError::PathHazardModuleId { module_id, reason });
        }
        let mut inner = self.lock_inner()?;
        if inner.candidates.contains_key(&module_id) {
            return Err(RegistryError::DuplicateModuleId { module_id });
        }
        let ready = manifest.ready.unwrap_or(true);
        let registration = ModuleRegistration {
            manifest,
            ready,
            negotiated_ver,
            state: ChannelState::Active,
            connection_id,
            control_ops,
        };
        inner.candidates.insert(module_id, registration.clone());
        inner.notify_module_changed(&registration.manifest.module_id);
        Ok(registration)
    }

    /// Move the candidate for `module_id` into the active slot and demote the
    /// previous active registration, in one registry critical section.
    ///
    /// Returns `Ok(None)` when there is no candidate to promote. Bumps the
    /// catalog generation, because the listed registration for the id changed.
    pub fn promote_candidate(
        &self,
        module_id: &str,
    ) -> Result<Option<RegistryCutover>, RegistryError> {
        let mut inner = self.lock_inner()?;
        let Some(promoted) = inner.candidates.remove(module_id) else {
            return Ok(None);
        };
        let superseded = inner
            .modules
            .insert(module_id.to_string(), promoted.clone());
        if let Some(superseded) = superseded.clone() {
            inner.superseded.push(superseded);
        }
        inner.bump_generation();
        inner.notify_module_changed(module_id);
        Ok(Some(RegistryCutover {
            promoted,
            superseded,
        }))
    }

    /// The ACTIVE registration for `module_id`. Candidates and superseded
    /// incumbents are never returned: this is the lookup routing decisions use.
    pub fn get_module(&self, module_id: &str) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self.lock_inner()?.modules.get(module_id).cloned())
    }

    /// The swap candidate registered for `module_id`, if any.
    pub fn get_candidate(
        &self,
        module_id: &str,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self.lock_inner()?.candidates.get(module_id).cloned())
    }

    /// The registration held in one specific slot. See [`RegistrationSlot`].
    pub fn registration(
        &self,
        slot: RegistrationSlot<'_>,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        let inner = self.lock_inner()?;
        Ok(match slot {
            RegistrationSlot::Active(module_id) => inner.modules.get(module_id).cloned(),
            RegistrationSlot::Candidate(module_id) => inner.candidates.get(module_id).cloned(),
            RegistrationSlot::Connection(connection_id) => inner
                .find_by_connection(connection_id)
                .map(|(_, registration)| registration.clone()),
        })
    }

    pub fn active_registration_count(&self) -> Result<usize, RegistryError> {
        Ok(self.lock_inner()?.modules.len())
    }

    pub fn list_modules(&self) -> Result<(u64, Vec<ModuleRegistration>), RegistryError> {
        let inner = self.lock_inner()?;
        let mut modules = inner.modules.values().cloned().collect::<Vec<_>>();
        modules.sort_by(|left, right| left.manifest.module_id.cmp(&right.manifest.module_id));
        Ok((inner.generation, modules))
    }

    pub fn generation(&self) -> Result<u64, RegistryError> {
        Ok(self.lock_inner()?.generation)
    }

    #[cfg(test)]
    pub(crate) fn set_module_state_for_test(
        &self,
        module_id: &str,
        state: ChannelState,
    ) -> Result<bool, RegistryError> {
        let mut inner = self.lock_inner()?;
        let Some(registration) = inner.modules.get_mut(module_id) else {
            return Ok(false);
        };
        registration.state = state;
        inner.notify_module_changed(module_id);
        Ok(true)
    }

    /// The registration owned by `connection_id`, searching the active,
    /// candidate and superseded slots in that order.
    pub fn get_module_by_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self
            .lock_inner()?
            .find_by_connection(connection_id)
            .map(|(_, registration)| registration.clone()))
    }

    /// Replace the provider role list and, when supplied, the attested capability
    /// declaration for the module owned by `connection_id`.
    ///
    /// Searches every slot: a swap candidate declares itself ready through this
    /// call, and if only the active slot were searched its update would find
    /// nothing and the candidate would never become ready. Only a change to the
    /// active slot bumps the catalog generation, because only the active slot is
    /// listed.
    pub fn replace_catalog_for_connection(
        &self,
        connection_id: ConnectionId,
        provides: Vec<ProviderRole>,
        capabilities: Option<CapabilityDeclarations>,
        ready: Option<bool>,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        let mut inner = self.lock_inner()?;
        let Some((slot, _)) = inner.find_by_connection(connection_id) else {
            return Ok(None);
        };
        let registration = inner
            .registration_mut(slot, connection_id)
            .expect("registration discovered under the same registry lock must still exist");
        registration.manifest.provides = provides;
        if let Some(capabilities) = capabilities {
            registration.manifest.capabilities = Some(capabilities);
        }
        if let Some(ready) = ready {
            registration.ready = ready;
            registration.manifest.ready = Some(ready);
        }
        let updated = registration.clone();
        if matches!(slot, SlotKind::Active) {
            inner.bump_generation();
        }
        inner.notify_module_changed(&updated.manifest.module_id);
        Ok(Some(updated))
    }

    /// Deregister every module owned by a dropped connection, in any slot.
    pub fn deregister_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<ModuleRegistration>, RegistryError> {
        let mut inner = self.lock_inner()?;
        let module_ids: Vec<String> = inner
            .modules
            .iter()
            .filter(|(_, registration)| registration.connection_id == connection_id)
            .map(|(module_id, _)| module_id.clone())
            .collect();

        let mut closed: Vec<ModuleRegistration> = module_ids
            .into_iter()
            .filter_map(|module_id| inner.close_module(&module_id))
            .collect();

        let candidate_ids: Vec<String> = inner
            .candidates
            .iter()
            .filter(|(_, registration)| registration.connection_id == connection_id)
            .map(|(module_id, _)| module_id.clone())
            .collect();
        for module_id in candidate_ids {
            if let Some(mut registration) = inner.candidates.remove(&module_id) {
                registration.state = ChannelState::Closed;
                closed.push(registration);
            }
        }

        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut inner.superseded)
            .into_iter()
            .partition(|registration| registration.connection_id == connection_id);
        inner.superseded = kept;
        closed.extend(removed.into_iter().map(|mut registration| {
            registration.state = ChannelState::Closed;
            registration
        }));
        for registration in &closed {
            inner.notify_module_changed(&registration.manifest.module_id);
        }
        Ok(closed)
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, RegistryInner>, RegistryError> {
        self.inner.lock().map_err(|_| RegistryError::Poisoned)
    }
}

/// Which internal slot a connection-keyed lookup found its registration in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Active,
    Candidate,
    Superseded,
}

impl RegistryInner {
    fn notify_module_changed(&self, module_id: &str) {
        if let Some(sender) = self.module_changes.get(module_id) {
            sender.send_replace(());
        }
    }

    fn find_by_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Option<(SlotKind, &ModuleRegistration)> {
        let owned_by =
            |registration: &&ModuleRegistration| registration.connection_id == connection_id;
        self.modules
            .values()
            .find(owned_by)
            .map(|registration| (SlotKind::Active, registration))
            .or_else(|| {
                self.candidates
                    .values()
                    .find(owned_by)
                    .map(|registration| (SlotKind::Candidate, registration))
            })
            .or_else(|| {
                self.superseded
                    .iter()
                    .find(owned_by)
                    .map(|registration| (SlotKind::Superseded, registration))
            })
    }

    fn registration_mut(
        &mut self,
        slot: SlotKind,
        connection_id: ConnectionId,
    ) -> Option<&mut ModuleRegistration> {
        let owned_by =
            |registration: &&mut ModuleRegistration| registration.connection_id == connection_id;
        match slot {
            SlotKind::Active => self.modules.values_mut().find(owned_by),
            SlotKind::Candidate => self.candidates.values_mut().find(owned_by),
            SlotKind::Superseded => self.superseded.iter_mut().find(owned_by),
        }
    }

    fn close_module(&mut self, module_id: &str) -> Option<ModuleRegistration> {
        let mut registration = self.modules.remove(module_id)?;
        registration.state = ChannelState::Closed;
        self.bump_generation();
        Some(registration)
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    DuplicateModuleId {
        module_id: String,
    },
    /// The id is unusable as a single path component. Enforced at
    /// registration because the daemon MINTS A STORAGE DESCRIPTOR from the
    /// self-claimed id verbatim (`<data_home>/cortexkit/<module_id>/store.db`),
    /// so an id carrying separators or dot components is a path-traversal or
    /// store-collision primitive handed to whoever claims it (issue #32). The
    /// derivations deliberately do NOT sanitize instead: sanitizing here would
    /// silently re-path every deployed store and desynchronize the Rust and TS
    /// derivations, while refusal changes nothing for any id that ever worked.
    PathHazardModuleId {
        module_id: String,
        reason: String,
    },
    Poisoned,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModuleId { module_id } => {
                write!(f, "module_id '{module_id}' is already registered")
            }
            Self::PathHazardModuleId { module_id, reason } => {
                write!(
                    f,
                    "module_id '{}' is not usable as a path component: {reason}",
                    module_id.escape_debug()
                )
            }
            Self::Poisoned => write!(f, "registry lock was poisoned"),
        }
    }
}

impl Error for RegistryError {}

/// Why `module_id` cannot serve as a single path component, or `Ok(())`.
///
/// This is a REFUSAL predicate, not a sanitizer: every currently-working fleet
/// id passes untouched, and anything refused here never worked meaningfully --
/// it either escaped `<data_home>/cortexkit/` (separators, dot components) or
/// aliased another module's store (`a/b` vs `a//b` collapsing on POSIX).
/// Colons are allowed: reserved-namespace children (`mcp:...`) register today
/// and a colon cannot traverse. Windows path legality is the store library's
/// concern, not an identity rule.
pub fn module_id_path_hazard(module_id: &str) -> Result<(), String> {
    if module_id.is_empty() {
        return Err("empty".to_string());
    }
    if module_id.contains('/') || module_id.contains('\\') {
        return Err("contains a path separator".to_string());
    }
    if module_id == "." || module_id == ".." {
        return Err("is a dot path component".to_string());
    }
    if module_id.chars().any(|c| c.is_control()) {
        return Err("contains a control character".to_string());
    }
    // `module_id` is already one literal store-path component, so this reports
    // the existing failure before derivation instead of adding a restriction.
    // NAME_MAX is 255 UTF-8 bytes, which `str::len()` measures.
    if module_id.len() > 255 {
        return Err("is longer than 255 bytes".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod change_notification_tests {
    use super::*;

    fn manifest() -> ModuleManifest {
        ModuleManifest::builder("watched", "1.0.0")
            .protocol_ver(1)
            .build()
    }

    fn changed(receiver: &mut watch::Receiver<()>) {
        assert!(
            receiver.has_changed().unwrap(),
            "registry write must notify"
        );
        receiver.borrow_and_update();
    }

    #[test]
    fn every_registration_write_notifies_only_its_module() {
        let registry = Registry::default();
        let mut events = registry.subscribe_module_changes("watched").unwrap();
        let other = registry.subscribe_module_changes("other").unwrap();
        let active = ConnectionId::new(1);
        let candidate = ConnectionId::new(2);
        registry
            .register_with_control_ops(manifest(), 1, active, vec![])
            .unwrap();
        changed(&mut events);
        registry
            .set_module_state_for_test("watched", ChannelState::Active)
            .unwrap();
        changed(&mut events);
        registry
            .replace_catalog_for_connection(active, vec![], None, Some(true))
            .unwrap();
        changed(&mut events);
        registry
            .register_candidate_with_control_ops(manifest(), 1, candidate, vec![])
            .unwrap();
        changed(&mut events);
        registry
            .replace_catalog_for_connection(candidate, vec![], None, Some(true))
            .unwrap();
        changed(&mut events);
        registry.promote_candidate("watched").unwrap().unwrap();
        changed(&mut events);
        // Superseded connections can still update and deregister their slot.
        registry
            .replace_catalog_for_connection(active, vec![], None, Some(false))
            .unwrap();
        changed(&mut events);
        registry.deregister_connection(active).unwrap();
        changed(&mut events);
        registry.deregister_connection(candidate).unwrap();
        changed(&mut events);
        registry
            .register_candidate_with_control_ops(manifest(), 1, candidate, vec![])
            .unwrap();
        changed(&mut events);
        registry.deregister_connection(candidate).unwrap();
        changed(&mut events);
        assert!(
            !other.has_changed().unwrap(),
            "other module must stay parked"
        );
    }

    #[test]
    fn subscription_before_read_keeps_a_change_until_waited_on() {
        let registry = Registry::default();
        let mut events = registry.subscribe_module_changes("watched").unwrap();
        assert!(registry.get_module("watched").unwrap().is_none());
        registry
            .register_with_control_ops(manifest(), 1, ConnectionId::new(1), vec![])
            .unwrap();
        // Neither a later lookup nor a coalesced catalog update consumes it.
        registry.get_module("watched").unwrap().unwrap();
        registry
            .replace_catalog_for_connection(ConnectionId::new(1), vec![], None, Some(true))
            .unwrap();
        changed(&mut events);
        assert!(!events.has_changed().unwrap());
    }
}

#[cfg(test)]
mod path_hazard_tests {
    use super::*;
    use crate::ConnectionId;
    use subc_protocol::manifest::ModuleManifest;

    fn manifest(module_id: &str) -> ModuleManifest {
        ModuleManifest::builder(module_id, "0.1.0")
            .protocol_ver(1)
            .build()
    }

    #[test]
    fn path_hazard_ids_are_refused_and_nothing_registers() {
        let registry = Registry::default();
        for (bad, reason_fragment) in [
            ("../escape", "path separator"),
            ("a/b", "path separator"),
            ("a\\b", "path separator"),
            ("..", "dot path component"),
            (".", "dot path component"),
            ("", "empty"),
            ("evil\u{0}id", "control character"),
        ] {
            let err = registry
                .register_with_control_ops(manifest(bad), 1, ConnectionId::new(7), Vec::new())
                .expect_err("path-hazard id must refuse");
            // Reason asserted so a predicate throwing the WRONG refusal fails.
            assert!(
                err.to_string().contains(reason_fragment),
                "id {bad:?}: expected {reason_fragment:?} in {err}"
            );
        }
        // THE EFFECT, not just the verdicts: no refusal left a registration
        // behind, and the generation never moved.
        assert_eq!(registry.active_registration_count().unwrap(), 0);
        assert_eq!(registry.generation().unwrap(), 0);
    }

    #[test]
    fn module_id_path_component_length_matches_shared_refusal_vectors() {
        let doc: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/golden/module_id_path_component_refusals.json"
        ))
        .expect("refusal fixture parses");

        for case in doc["vectors"].as_array().expect("vectors array") {
            let name = case["name"].as_str().expect("name");
            let module_id = case["module_id"]["unit"]
                .as_str()
                .expect("module_id unit")
                .repeat(
                    case["module_id"]["repeat"]
                        .as_u64()
                        .expect("module_id repeat") as usize,
                );
            assert_eq!(
                module_id.len(),
                case["utf8_bytes"].as_u64().expect("utf8 bytes") as usize
            );

            let expected = case["expect_reason"].as_str().map(str::to_owned);
            assert_eq!(
                module_id_path_hazard(&module_id).err(),
                expected,
                "shared refusal vector {name:?} diverged"
            );
        }
    }

    #[test]
    fn working_id_shapes_register_including_namespace_colons() {
        let registry = Registry::default();
        for (i, good) in ["magic-context", "mcp:everything", "v1.2-module"]
            .iter()
            .enumerate()
        {
            registry
                .register_with_control_ops(
                    manifest(good),
                    1,
                    ConnectionId::new(10 + i as u64),
                    Vec::new(),
                )
                .unwrap_or_else(|err| panic!("id {good:?} must register: {err}"));
        }
        assert_eq!(registry.active_registration_count().unwrap(), 3);
    }
}

#[cfg(test)]
mod swap_slot_tests {
    use super::*;

    fn manifest(module_id: &str, ready: Option<bool>) -> ModuleManifest {
        let mut manifest = ModuleManifest::builder(module_id, "0.1.0").build();
        manifest.ready = ready;
        manifest
    }

    const INCUMBENT: ConnectionId = ConnectionId(1);
    const CANDIDATE: ConnectionId = ConnectionId(2);

    fn registry_with_candidate() -> Registry {
        let registry = Registry::default();
        registry
            .register_with_control_ops(manifest("m", None), 1, INCUMBENT, Vec::new())
            .unwrap();
        registry
            .register_candidate_with_control_ops(
                manifest("m", Some(false)),
                1,
                CANDIDATE,
                Vec::new(),
            )
            .unwrap();
        registry
    }

    #[test]
    fn candidate_is_invisible_to_by_id_lookups_and_listing() {
        let registry = registry_with_candidate();
        let generation = registry.generation().unwrap();
        assert_eq!(
            registry.get_module("m").unwrap().unwrap().connection_id,
            INCUMBENT
        );
        let (listed_generation, listed) = registry.list_modules().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].connection_id, INCUMBENT);
        assert_eq!(listed_generation, generation);
        assert_eq!(registry.active_registration_count().unwrap(), 1);
        assert_eq!(
            registry.get_candidate("m").unwrap().unwrap().connection_id,
            CANDIDATE
        );
        assert_eq!(
            registry
                .register_candidate_with_control_ops(
                    manifest("m", None),
                    1,
                    ConnectionId(3),
                    Vec::new()
                )
                .unwrap_err(),
            RegistryError::DuplicateModuleId {
                module_id: "m".to_string()
            }
        );
    }

    /// Without the candidate slot in the connection-keyed search, this update
    /// returns `Ok(None)` and the candidate never becomes ready.
    #[test]
    fn candidate_catalog_update_reaches_the_candidate_registration() {
        let registry = registry_with_candidate();
        assert!(!registry.get_candidate("m").unwrap().unwrap().ready);

        let updated = registry
            .replace_catalog_for_connection(CANDIDATE, Vec::new(), None, Some(true))
            .unwrap()
            .expect("the candidate's own connection finds its registration");

        assert_eq!(updated.connection_id, CANDIDATE);
        assert!(registry.get_candidate("m").unwrap().unwrap().ready);
        assert_eq!(
            registry.get_module_by_connection(CANDIDATE).unwrap(),
            Some(updated)
        );
        assert_eq!(
            registry.get_module("m").unwrap().unwrap().connection_id,
            INCUMBENT,
            "a candidate's update must not touch the active registration"
        );
    }

    #[test]
    fn promotion_swaps_slots_and_each_connection_still_deregisters_its_own() {
        let registry = registry_with_candidate();
        let before = registry.generation().unwrap();
        let cutover = registry.promote_candidate("m").unwrap().unwrap();
        assert_eq!(cutover.promoted.connection_id, CANDIDATE);
        assert_eq!(cutover.superseded.unwrap().connection_id, INCUMBENT);
        assert_ne!(registry.generation().unwrap(), before);
        assert_eq!(registry.promote_candidate("m").unwrap(), None);

        assert_eq!(
            registry
                .registration(RegistrationSlot::Active("m"))
                .unwrap()
                .unwrap()
                .connection_id,
            CANDIDATE
        );
        assert!(registry
            .registration(RegistrationSlot::Candidate("m"))
            .unwrap()
            .is_none());
        assert!(registry
            .registration(RegistrationSlot::Connection(INCUMBENT))
            .unwrap()
            .is_some());

        let closed = registry.deregister_connection(INCUMBENT).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].connection_id, INCUMBENT);
        assert_eq!(closed[0].state, ChannelState::Closed);
        assert!(registry
            .registration(RegistrationSlot::Connection(INCUMBENT))
            .unwrap()
            .is_none());
        assert_eq!(
            registry.get_module("m").unwrap().unwrap().connection_id,
            CANDIDATE
        );
    }

    #[test]
    fn a_dropped_candidate_deregisters_from_the_candidate_slot_only() {
        let registry = registry_with_candidate();
        let closed = registry.deregister_connection(CANDIDATE).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].connection_id, CANDIDATE);
        assert!(registry.get_candidate("m").unwrap().is_none());
        assert_eq!(
            registry.get_module("m").unwrap().unwrap().connection_id,
            INCUMBENT
        );
    }
}
