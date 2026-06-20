use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

use subc_protocol::manifest::ModuleManifest;

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
        let mut inner = self.lock_inner()?;
        let module_id = manifest.module_id.clone();
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
    DuplicateModuleId { module_id: String },
    Poisoned,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModuleId { module_id } => {
                write!(f, "module_id '{module_id}' is already registered")
            }
            Self::Poisoned => write!(f, "registry lock was poisoned"),
        }
    }
}

impl Error for RegistryError {}
