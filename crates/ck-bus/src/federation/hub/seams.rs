//! What the hub area reaches outside itself, each behind a trait so the unit tests can
//! stand in for callosum, the hub instance and this machine's identity.

use async_trait::async_trait;
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
    /// `HUB_INBOX_{box}`, one per machine, created when that machine's first leaf JWT
    /// is issued.
    Inbox {
        box_id: String,
    },
    /// The account-wide shared identity log, `HUB_IDLOG`.
    IdLog,
    /// The account-wide residence records, the KV bucket `residence` (`KV_residence`).
    Residence,
    /// The account-wide move cuts, the object store `cuts` (`OBJ_cuts`).
    Cuts,
    Rooms,
    Memory,
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
    /// Creates each stream that does not exist yet; an existing one is left as it is.
    async fn ensure_streams(&self, record: &HubRecord, streams: &[HubStream])
        -> Result<(), String>;
    /// The leaf connections bound to the hub account.
    async fn leaf_count(&self, record: &HubRecord) -> Result<usize, String>;
}

/// This machine's own identity, which its own leaf credential names.
#[async_trait]
pub trait OwnMachine: Send + Sync {
    /// The machine id, the `{box}` of this machine's own grant.
    async fn machine_id(&self) -> Result<String, String>;
}
