mod seams;
mod store;

use std::{path::PathBuf, sync::Arc};

pub use seams::{
    Census, CredentialMinting, GrantGeneration, LeafConfiguration, Revocation, SentinelHealth,
    SpawnStream,
};
pub use seams::{GeneratedGrant, GrantSeamError, OwnUser, SeamResult};
pub use store::resolve_store_root;

/// One implementation per runtime capability, each initially an explicit not-implemented placeholder.
pub struct Runtime {
    store_root: PathBuf,
    pub credentials: Arc<dyn CredentialMinting>,
    pub census: Arc<dyn Census>,
    pub revocation: Arc<dyn Revocation>,
    pub grants: Arc<dyn GrantGeneration>,
    pub spawn_stream: Arc<dyn SpawnStream>,
    pub leaf: Arc<dyn LeafConfiguration>,
    pub sentinel_health: Arc<dyn SentinelHealth>,
}

impl Runtime {
    pub fn refusing_defaults(store_root: PathBuf) -> Self {
        Self {
            store_root,
            credentials: seams::refusing_credential_minting(),
            census: seams::refusing_census(),
            revocation: seams::refusing_revocation(),
            grants: seams::refusing_grant_generation(),
            spawn_stream: seams::refusing_spawn_stream(),
            leaf: seams::refusing_leaf_configuration(),
            sentinel_health: seams::refusing_sentinel_health(),
        }
    }

    pub fn store_root(&self) -> &PathBuf {
        &self.store_root
    }

    pub fn with_credentials(mut self, implementation: Arc<dyn CredentialMinting>) -> Self {
        self.credentials = implementation;
        self
    }

    pub fn with_census(mut self, implementation: Arc<dyn Census>) -> Self {
        self.census = implementation;
        self
    }

    pub fn with_revocation(mut self, implementation: Arc<dyn Revocation>) -> Self {
        self.revocation = implementation;
        self
    }

    pub fn with_grants(mut self, implementation: Arc<dyn GrantGeneration>) -> Self {
        self.grants = implementation;
        self
    }

    pub fn with_spawn_stream(mut self, implementation: Arc<dyn SpawnStream>) -> Self {
        self.spawn_stream = implementation;
        self
    }

    pub fn with_leaf(mut self, implementation: Arc<dyn LeafConfiguration>) -> Self {
        self.leaf = implementation;
        self
    }

    pub fn with_sentinel_health(mut self, implementation: Arc<dyn SentinelHealth>) -> Self {
        self.sentinel_health = implementation;
        self
    }
}
