//! What the hub area reaches outside itself, each behind a trait so the unit tests can
//! stand in for callosum, the hub instance and this machine's identity.

use std::collections::BTreeMap;

use async_trait::async_trait;
use cortexkit_bus_naming::federation::HubName;
use tokio::sync::mpsc;

use super::record::HubRecord;

/// Callosum's paired-device ops, which it serves to `reserved:ckbus`.
#[async_trait]
pub trait PairedDevices: Send + Sync {
    /// Opens `callosum.paired_devices_watch`; each notice body arrives on the receiver,
    /// and the receiver closing means the watch ended.
    async fn watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String>;
    /// One `callosum.paired_devices_read` body.
    async fn read(&self) -> Result<Vec<u8>, String>;
    /// One `callosum.peer_keys_read` body.
    async fn peer_keys(&self) -> Result<Vec<u8>, String>;
}

/// A stream the hub's ck-bus creates in the hub account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubStream {
    /// The inbox for a machine under the hub's current key.
    Inbox {
        box_id: String,
        hub: HubName,
    },
    /// The account-wide shared identity log, `HUB_IDLOG`.
    IdLog,
    /// The account-wide residence records, the KV bucket `residence` (`KV_residence`).
    Residence,
    /// The account-wide move cuts, the object store `cuts` (`OBJ_cuts`).
    Cuts,
    Rooms,
    Memory,
    VaultSharedLogins,
    VaultManifestActive,
}

/// The hub instance, through ck-bus's own users on its loopback admin listener.
#[async_trait]
pub trait HubServer: Send + Sync {
    /// Pushes an account JWT through the hub's system account and reads it back. The
    /// server closes every connection of the account whose JWT the new revocations
    /// cover, leaf connections included.
    async fn apply_account(
        &self,
        record: &HubRecord,
        account_public: &str,
        jwt: &str,
    ) -> Result<(), String>;
    /// Reconciles streams in place. For an inbox, returns the previous hub names
    /// that still hold messages and must be included in the box's leaf grant.
    async fn ensure_streams(
        &self,
        record: &HubRecord,
        streams: &[HubStream],
    ) -> Result<BTreeMap<String, Vec<HubName>>, String>;
    /// The leaf connections bound to the hub account.
    async fn leaf_count(&self, record: &HubRecord) -> Result<usize, String>;
}

/// This machine's own identity, which its own leaf credential names.
#[async_trait]
pub trait OwnMachine: Send + Sync {
    /// The machine id, the `{box}` of this machine's own grant.
    async fn machine_id(&self) -> Result<String, String>;
}
