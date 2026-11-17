//! What the leaf area reaches outside itself, each behind a trait so the unit tests can
//! stand in for callosum, the hub, the local server and the outbox.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::bootstrap::plane::LeafLink;

/// Callosum's hub row and its relay bridge.
#[async_trait]
pub trait Callosum: Send + Sync {
    /// Opens `callosum.hub_watch`. Each notice body arrives on the receiver; the
    /// receiver closing means the watch ended.
    async fn hub_watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String>;
    /// One `callosum.hub_read` body.
    async fn hub_read(&self) -> Result<Vec<u8>, String>;
    /// `callosum.leaf_bridge_open {fingerprint}`: a loopback port that carries the leaf
    /// link through callosum's session with the hub computer.
    async fn leaf_bridge_open(&self, fingerprint: &str) -> Result<u16, String>;
}

/// The hub computer's ck-bus, reached through callosum's re-export of its shared ops.
#[async_trait]
pub trait PeerIssuer: Send + Sync {
    /// `ckbus.hub_leaf_issue {leaf_nkey}` on module `fed:<hub fingerprint>:ckbus`: the
    /// leaf user JWT for `leaf_nkey`.
    async fn hub_leaf_issue(
        &self,
        hub_fingerprint: &str,
        leaf_nkey: &str,
    ) -> Result<String, String>;
}

/// Where a self hub listens and what it presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHubIdentity {
    /// SHA-256 of the hub certificate's SubjectPublicKeyInfo, 64 lowercase hex.
    pub spki_sha256: String,
    pub leaf_port: u16,
}

/// The hub this machine runs, when it is the hub. Its side of ck-bus implements this.
#[async_trait]
pub trait LocalHub: Send + Sync {
    async fn identity(&self) -> Result<LocalHubIdentity, String>;
    /// Issues this machine's own leaf user JWT, as `hub_leaf_issue` does for a peer.
    async fn issue(&self, leaf_nkey: &str) -> Result<String, String>;
}

/// A local hub that never answers, for the tests that run a leaf area without the hub
/// side.
pub struct NoLocalHub;

const NO_LOCAL_HUB: &str = "this ck-bus runs no hub";

#[async_trait]
impl LocalHub for NoLocalHub {
    async fn identity(&self) -> Result<LocalHubIdentity, String> {
        Err(NO_LOCAL_HUB.to_string())
    }

    async fn issue(&self, _leaf_nkey: &str) -> Result<String, String> {
        Err(NO_LOCAL_HUB.to_string())
    }
}

/// The local server's view of the leaf connection, through the system account.
#[async_trait]
pub trait LeafLinks: Send + Sync {
    /// The leaf connections bound to the federation account `account`.
    async fn leafs(&self, account: &str) -> Result<Vec<LeafLink>, String>;
    /// Disconnects one leaf connection, so the engine reconnects with a fresh signature.
    async fn kick(&self, link: &LeafLink) -> Result<(), String>;
}

/// Whether ck-bus has an outbound frame in flight on the leaf. A renewal kick waits for
/// a quiet moment so it does not cut a delivery mid-flight.
pub trait OutboundQuiet: Send + Sync {
    /// True when no outbound frame has been in flight for at least `window`.
    fn quiet_for(&self, window: Duration) -> bool;
}

/// Nothing sends on the leaf yet, so it is always quiet.
pub struct AlwaysQuiet;

impl OutboundQuiet for AlwaysQuiet {
    fn quiet_for(&self, _window: Duration) -> bool {
        true
    }
}

/// The federation account on the local server.
#[async_trait]
pub trait FederationAccounts: Send + Sync {
    /// Finds or creates the account and returns its public key.
    async fn ensure(&self) -> Result<String, String>;
}
