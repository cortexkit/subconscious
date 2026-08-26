use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

use subc_protocol::manifest::{CapabilityDeclarations, ModuleManifest, ProviderRole};

/// Per-connection identity assigned by [`crate::Router`] while serving a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Synthetic connection id used by unit tests. Router-issued connection ids
    /// start at 1, so this 0 value never collides with a real socket owner.
    #[cfg(test)]
    pub const LOCAL: Self = Self(0);

    pub fn new(raw: u64) -> Self {
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
    pub negotiated_ver: u8,
    pub state: ChannelState,
    pub connection_id: ConnectionId,
    pub control_ops: Vec<String>,
}

/// Control-plane registry for module manifests and supervision ownership.
///
/// Duplicate active `module_id`s are rejected rather than replaced. Rejection is
/// the safer v1 behavior because replacing a still-connected module could hijack
/// in-flight routes. Stale registrations are removed by connection cleanup; a
/// reconnect after the old connection drops can then register the same id again.
#[derive(Debug, Default)]
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    modules: HashMap<String, ModuleRegistration>,
    generation: u64,
}

impl Registry {
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

        let registration = ModuleRegistration {
            manifest,
            negotiated_ver,
            state: ChannelState::Active,
            connection_id,
            control_ops,
        };

        inner.modules.insert(module_id, registration.clone());
        inner.bump_generation();
        Ok(registration)
    }

    pub fn get_module(&self, module_id: &str) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self.lock_inner()?.modules.get(module_id).cloned())
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
        Ok(true)
    }

    pub fn get_module_by_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self
            .lock_inner()?
            .modules
            .values()
            .find(|registration| registration.connection_id == connection_id)
            .cloned())
    }

    /// Replace the provider role list and, when supplied, the attested capability
    /// declaration for the module owned by `connection_id`.
    pub fn replace_catalog_for_connection(
        &self,
        connection_id: ConnectionId,
        provides: Vec<ProviderRole>,
        capabilities: Option<CapabilityDeclarations>,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        let mut inner = self.lock_inner()?;
        let Some(module_id) = inner
            .modules
            .iter()
            .find(|(_, registration)| registration.connection_id == connection_id)
            .map(|(module_id, _)| module_id.clone())
        else {
            return Ok(None);
        };

        let registration = inner
            .modules
            .get_mut(&module_id)
            .expect("module_id discovered from registry values must still exist");
        registration.manifest.provides = provides;
        if let Some(capabilities) = capabilities {
            registration.manifest.capabilities = Some(capabilities);
        }
        let updated = registration.clone();
        inner.bump_generation();
        Ok(Some(updated))
    }

    /// Deregister every module owned by a dropped connection.
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

        Ok(module_ids
            .into_iter()
            .filter_map(|module_id| inner.close_module(&module_id))
            .collect())
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, RegistryInner>, RegistryError> {
        self.inner.lock().map_err(|_| RegistryError::Poisoned)
    }
}

impl RegistryInner {
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
    Ok(())
}

#[cfg(test)]
mod path_hazard_tests {
    use super::*;
    use crate::ConnectionId;
    use subc_protocol::manifest::{
        Bindings, IdentityBinding, IdentityScope, ModuleManifest, StorageBinding, StorageKind,
        StorageScope, TrustTier,
    };

    fn manifest(module_id: &str) -> ModuleManifest {
        ModuleManifest {
            module_id: module_id.to_string(),
            module_version: "0.1.0".to_string(),
            protocol_ver: 1,
            trust_tier: TrustTier::FirstParty,
            provides: Vec::new(),
            consumes: Vec::new(),
            bindings: Bindings {
                storage: StorageBinding {
                    kind: StorageKind::Sqlite,
                    scope: StorageScope::Project,
                    owns_schema: true,
                },
                vault_grants: Vec::new(),
                identity: IdentityBinding {
                    requires: vec![IdentityScope::Project],
                    optional: Vec::new(),
                },
            },
            capabilities: None,
            provenance: None,
        }
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
