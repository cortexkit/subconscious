use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

use subc_protocol::manifest::ModuleManifest;

/// First dynamically allocated module data-plane channel.
///
/// Channel 0 is reserved for subc's control plane; module registrations receive
/// one non-zero channel for their v1 `(component, session)` routes.
pub const FIRST_MODULE_CHANNEL: u16 = 1;

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
    pub channels: Vec<u16>,
    pub state: ChannelState,
    pub connection_id: ConnectionId,
}

/// Control-plane registry for module manifests and channel ownership.
///
/// Duplicate active `module_id`s are rejected rather than replaced. Rejection is
/// the safer v1 behavior because replacing a still-connected module could hijack
/// in-flight routes. Stale registrations are removed by connection cleanup; a
/// reconnect after the old connection drops can then register the same id again.
#[derive(Debug, Default)]
pub struct Registry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug)]
struct RegistryInner {
    modules: HashMap<String, ModuleRegistration>,
    channels: HashMap<u16, String>,
    next_channel: u16,
    generation: u64,
}

impl Default for RegistryInner {
    fn default() -> Self {
        Self {
            modules: HashMap::new(),
            channels: HashMap::new(),
            next_channel: FIRST_MODULE_CHANNEL,
            generation: 0,
        }
    }
}

impl Registry {
    /// Register a module manifest, negotiate a single v1 route channel, and mark
    /// the registration active once allocation succeeds.
    pub fn register(
        &self,
        manifest: ModuleManifest,
        negotiated_ver: u8,
        connection_id: ConnectionId,
    ) -> Result<ModuleRegistration, RegistryError> {
        let mut inner = self.lock_inner()?;
        let module_id = manifest.module_id.clone();
        if inner.modules.contains_key(&module_id) {
            return Err(RegistryError::DuplicateModuleId { module_id });
        }

        let channel = inner.allocate_channel()?;
        let registration = ModuleRegistration {
            manifest,
            negotiated_ver,
            channels: vec![channel],
            state: ChannelState::Active,
            connection_id,
        };

        inner.channels.insert(channel, module_id.clone());
        inner.modules.insert(module_id, registration.clone());
        inner.bump_generation();
        Ok(registration)
    }

    pub fn get_module(&self, module_id: &str) -> Result<Option<ModuleRegistration>, RegistryError> {
        Ok(self.lock_inner()?.modules.get(module_id).cloned())
    }

    pub fn module_for_channel(
        &self,
        channel: u16,
    ) -> Result<Option<ModuleRegistration>, RegistryError> {
        let inner = self.lock_inner()?;
        Ok(inner
            .channels
            .get(&channel)
            .and_then(|module_id| inner.modules.get(module_id))
            .cloned())
    }

    pub fn is_channel_active(&self, channel: u16) -> Result<bool, RegistryError> {
        Ok(self.module_for_channel(channel)?.is_some())
    }

    pub fn active_module_count(&self) -> Result<usize, RegistryError> {
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
    fn allocate_channel(&mut self) -> Result<u16, RegistryError> {
        // Worst case under full allocation scans every non-zero u16 channel once
        // before reporting exhaustion.
        let mut candidate = self.next_channel;
        for _ in FIRST_MODULE_CHANNEL..=u16::MAX {
            if candidate == 0 {
                candidate = FIRST_MODULE_CHANNEL;
            }
            if !self.channels.contains_key(&candidate) {
                self.next_channel = candidate.wrapping_add(1);
                if self.next_channel == 0 {
                    self.next_channel = FIRST_MODULE_CHANNEL;
                }
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }

        Err(RegistryError::ChannelExhausted)
    }

    fn close_module(&mut self, module_id: &str) -> Option<ModuleRegistration> {
        let mut registration = self.modules.remove(module_id)?;
        for channel in &registration.channels {
            self.channels.remove(channel);
        }
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
    ChannelExhausted,
    Poisoned,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModuleId { module_id } => {
                write!(f, "module_id '{module_id}' is already registered")
            }
            Self::ChannelExhausted => write!(f, "no module channels are available"),
            Self::Poisoned => write!(f, "registry lock was poisoned"),
        }
    }
}

impl Error for RegistryError {}
